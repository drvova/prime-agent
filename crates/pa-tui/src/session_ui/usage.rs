//! The `/usage` page's background fetches and keys. The page opens at once over the last fetched
//! halves; each `get_usage` half is a spawned request whose reply lands through the run loop's
//! channel, so neither a slow provider nor a long session scan ever holds a frame (AGENTS.md
//! "Performance and the critical path").

use pa_types::provider_usage::{QuotaReport, SpendHistory, UsageSection};

use super::{key_event_to_id, AgentView, DaemonCommand, Duration, KeyEvent, Map, SessionUi, Value};
use crate::usage_page::{
    countdown, next_label_change, now_ms, tightest_window, window_label, UsagePage, UsagePageAction,
};

/// The request bound: above the daemon's own per-provider bound, so a hung provider reaches the
/// page as its card's error rather than as a lost half.
const USAGE_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// One landed `get_usage` half.
#[derive(Debug)]
pub(crate) enum UsageFetched {
    Quotas(Result<QuotaReport, String>),
    /// Boxed: the history is far larger than the quota report.
    Spend(Result<Box<SpendHistory>, String>),
}

/// A landed half, tagged with the fetch generation that issued it.
#[derive(Debug)]
pub(crate) struct UsageUpdate {
    generation: u64,
    fetched: UsageFetched,
}

/// The session's last rate-limited response: its provider and when the provider said it resets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RateLimitReset {
    provider: Option<String>,
    resets_at_ms: i64,
}

/// A rate-limited assistant message's wait: its `provider_stream_failure` diagnostic with HTTP 429
/// (or the `rate_limit` kind) and the `retryAfterMs` the provider asked for.
fn rate_limit_wait_ms(message: &Value) -> Option<i64> {
    let details = message
        .get("diagnostics")?
        .as_array()?
        .iter()
        .find(|diagnostic| {
            diagnostic.get("type").and_then(Value::as_str) == Some("provider_stream_failure")
        })?
        .get("details")?;
    let limited = details.get("status").and_then(Value::as_u64) == Some(429)
        || details.get("kind").and_then(Value::as_str) == Some("rate_limit");
    limited
        .then(|| details.get("retryAfterMs").and_then(Value::as_i64))
        .flatten()
}

/// Whether a fetch may serve the daemon's cached quota cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaFreshness {
    Cached,
    Refetch,
}

impl SessionUi {
    /// Fold one settled assistant message into the tray's reset chip: a rate limit with a reset
    /// arms it (from now: the wait counts from the failure), a successful response clears it, an
    /// abort leaves it.
    pub(crate) fn observe_rate_limit(&mut self, message: &Value) {
        match message.get("stopReason").and_then(Value::as_str) {
            Some("aborted") => {}
            Some("error") => {
                if let Some(wait) = rate_limit_wait_ms(message) {
                    self.rate_limit_reset = Some(RateLimitReset {
                        provider: message
                            .get("provider")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        resets_at_ms: now_ms() + wait,
                    });
                }
            }
            Some(_) | None => self.rate_limit_reset = None,
        }
    }

    /// Re-derive the tray's reset chip at `now_ms` (the run loop calls this before each paint):
    /// shown while the current provider's reset is ahead, gone once it passes. A changed label
    /// dirties the frame; each appearance counts for the adoption event.
    pub(crate) fn refresh_reset_chip(&mut self, view: &mut AgentView, now_ms: i64) {
        let current_provider = view.chrome.model_provider.as_deref();
        let chip = match &self.rate_limit_reset {
            Some(reset) if reset.resets_at_ms <= now_ms => {
                self.rate_limit_reset = None;
                None
            }
            Some(reset)
                if reset.provider.is_none()
                    || current_provider.is_none()
                    || reset.provider.as_deref() == current_provider =>
            {
                Some(format!(
                    "limit resets in {}",
                    countdown(reset.resets_at_ms - now_ms)
                ))
            }
            Some(_) | None => None,
        };
        if chip != view.chrome.reset_chip {
            if view.chrome.reset_chip.is_none() {
                self.reset_chips_shown += 1;
            }
            view.chrome.reset_chip = chip;
            self.dirty = true;
        }
    }

    /// When the shown chip's label next changes (or it expires): the run loop's frame deadline.
    pub(crate) fn reset_chip_deadline(&self, view: &AgentView) -> Option<std::time::Instant> {
        let reset = self.rate_limit_reset.as_ref()?;
        view.chrome.reset_chip.as_ref()?;
        let now = now_ms();
        let wait =
            (next_label_change(now, reset.resets_at_ms).min(reset.resets_at_ms) - now).max(0);
        Some(std::time::Instant::now() + Duration::from_millis(wait.unsigned_abs()))
    }

    /// Re-derive the title's plan quota at `now_ms` from the cached `/usage` cards (the run loop
    /// calls this before each paint; nothing here fetches): the current provider's most-used
    /// window that has not reset yet, as `5h 42% · 1h 20m`.
    pub(crate) fn refresh_plan_quota(&mut self, view: &mut AgentView, now_ms: i64) {
        let provider = view.chrome.model_provider.as_deref();
        let tightest = self
            .usage_quotas
            .as_ref()
            .and_then(|report| {
                report
                    .providers
                    .iter()
                    .find(|card| Some(card.provider.as_str()) == provider)
            })
            .and_then(|card| tightest_window(card, now_ms));
        let quota = tightest.map(|(window, fraction)| {
            let label = window_label(window, fraction);
            match window.resets_at_ms {
                Some(at) => format!("{label} \u{b7} {}", countdown(at - now_ms)),
                None => label,
            }
        });
        self.plan_quota_resets_at_ms = tightest.and_then(|(window, _)| window.resets_at_ms);
        if quota != view.chrome.plan_quota {
            view.chrome.plan_quota = quota;
            self.dirty = true;
        }
    }

    /// When the title's plan quota countdown next changes (or its window resets).
    pub(crate) fn plan_quota_deadline(&self, view: &AgentView) -> Option<std::time::Instant> {
        let resets_at = self.plan_quota_resets_at_ms?;
        view.chrome.plan_quota.as_ref()?;
        let now = now_ms();
        let wait = (next_label_change(now, resets_at).min(resets_at) - now).max(0);
        Some(std::time::Instant::now() + Duration::from_millis(wait.unsigned_abs()))
    }

    /// Open the page over the last fetched halves and fetch both in the background.
    pub(crate) fn open_usage_page(&mut self, view: &mut AgentView) {
        view.usage_page = Some(UsagePage::new(
            self.usage_quotas.clone(),
            self.usage_spend.clone(),
        ));
        self.spawn_usage_fetches(QuotaFreshness::Cached);
        self.dirty = true;
    }

    /// Spawn both halves' requests under a new generation.
    fn spawn_usage_fetches(&mut self, freshness: QuotaFreshness) {
        self.usage_generation += 1;
        let generation = self.usage_generation;
        for section in [UsageSection::Spend, UsageSection::Quotas] {
            let client = self.client.clone();
            let updates = self.usage_updates.clone();
            tokio::spawn(async move {
                let request = DaemonCommand::GetUsage {
                    id: None,
                    section,
                    force_refresh: (freshness == QuotaFreshness::Refetch).then_some(true),
                    rest: Map::default(),
                };
                let data = match tokio::time::timeout(
                    Duration::from_millis(USAGE_REQUEST_TIMEOUT_MS),
                    client.request_ok(request),
                )
                .await
                {
                    Ok(Ok(data)) => Ok(data),
                    Ok(Err(error)) => Err(format!("{error:#}")),
                    Err(_) => Err("the daemon did not answer in time".to_string()),
                };
                let fetched = match section {
                    UsageSection::Quotas => UsageFetched::Quotas(data.and_then(|data| {
                        serde_json::from_value(data).map_err(|error| error.to_string())
                    })),
                    UsageSection::Spend => UsageFetched::Spend(data.and_then(|data| {
                        serde_json::from_value(data)
                            .map(Box::new)
                            .map_err(|error| error.to_string())
                    })),
                };
                let _ = updates.send(UsageUpdate {
                    generation,
                    fetched,
                });
            });
        }
    }

    /// Fold a landed half: a success refreshes the session's cache whatever its generation; the
    /// open page takes only its own generation's halves.
    pub(crate) fn apply_usage_update(&mut self, update: UsageUpdate, view: &mut AgentView) {
        let current = update.generation == self.usage_generation;
        match update.fetched {
            UsageFetched::Quotas(result) => {
                if let Ok(report) = &result {
                    self.usage_quotas = Some(report.clone());
                }
                if let Some(page) = view.usage_page.as_mut().filter(|_| current) {
                    page.apply_quotas(result);
                }
            }
            UsageFetched::Spend(result) => {
                let result = result.map(|history| *history);
                if let Ok(history) = &result {
                    self.usage_spend = Some(history.clone());
                }
                if let Some(page) = view.usage_page.as_mut().filter(|_| current) {
                    page.apply_spend(result);
                }
            }
        }
        self.dirty = true;
    }

    /// One key press while the page is open: refresh re-fetches both halves past the quota
    /// cache; the close emits the page's adoption event off the key path.
    pub(super) fn handle_usage_page_key(&mut self, key: KeyEvent, view: &mut AgentView) {
        let Some(id) = key_event_to_id(&key) else {
            return;
        };
        let Some(page) = view.usage_page.as_mut() else {
            return;
        };
        match page.handle_key(&id, view.editor.keybindings()) {
            UsagePageAction::None => {}
            UsagePageAction::Refresh => self.spawn_usage_fetches(QuotaFreshness::Refetch),
            UsagePageAction::Close => {
                let mut summary = page.summary();
                summary.reset_chip_shown_count = std::mem::take(&mut self.reset_chips_shown);
                view.usage_page = None;
                if let Some(telemetry) = self.telemetry.clone() {
                    tokio::spawn(async move {
                        telemetry.usage_page_closed(summary).await;
                    });
                }
            }
        }
        self.dirty = true;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::daemon_client::DaemonClient;
    use crate::interactive::SessionSelection;
    use crate::session_ui::{ActivityUpdates, SubmitBehavior};
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::mpsc;

    /// A mock daemon that attaches, answers the `spend` half, and never answers the `quotas`
    /// half: the hung provider endpoint, seen from the client.
    fn hanging_quota_daemon(listener: tokio::net::UnixListener) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            write.write_all(b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7},\"serverCapabilities\":[],\"clientId\":\"mock\"}\n").await.unwrap();
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                let kind = request["command"]["type"].as_str().unwrap();
                let data = match (kind, request["command"]["section"].as_str()) {
                    ("get_usage", Some("quotas")) => continue,
                    ("get_usage", _) => json!({
                        "today": {}, "last7Days": {}, "last30Days": {}, "allTime": {},
                        "daily": [], "models": [], "rateLimits": [], "sessions": 0,
                    }),
                    ("attach", _) => json!({
                        "activeSessionId": "s1", "snapshot": {
                            "activeSessionId": "s1", "summary": {"id": "s1", "cwd": "/tmp"},
                            "state": {"sessionId": "s1", "cwd": "/tmp", "isStreaming": false},
                            "messages": [], "lastEventSequence": 0,
                        }, "replay": {"status": "complete", "toSequence": 0},
                        "lastEventSequence": 0,
                    }),
                    (_, _) => json!({}),
                };
                let mut reply = serde_json::to_vec(&json!({
                    "type": "response", "id": request["id"], "command": kind,
                    "success": true, "data": data,
                }))
                .unwrap();
                reply.push(b'\n');
                write.write_all(&reply).await.unwrap();
            }
        })
    }

    fn frame_text(view: &mut AgentView) -> Vec<String> {
        view.render_frame(80, 30)
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| -> &str { span.content.as_ref() })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// AGENTS.md "Performance and the critical path": a quota endpoint that never answers must
    /// not delay the page's first frame. The `/usage` dispatch returns while the quota half
    /// hangs, the first frame paints the loading row, and the spend half still lands beside it.
    #[tokio::test]
    async fn a_hanging_quota_fetch_never_delays_the_pages_first_frame() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("usage.sock");
            let server = hanging_quota_daemon(tokio::net::UnixListener::bind(&socket).unwrap());
            let mut options =
                crate::interactive::tests::options(crate::interactive::ModelSelection::default());
            options.socket_path = socket.clone();
            options.session = SessionSelection::Attach("s1".to_string());
            let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
            let (usage_tx, mut usage_rx) = mpsc::unbounded_channel();
            let mut session = SessionUi::open(
                client,
                &options,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                ActivityUpdates {
                    heartbeats: mpsc::unbounded_channel().0,
                    bash: mpsc::unbounded_channel().0,
                    factory: mpsc::unbounded_channel().0,
                    commands: mpsc::unbounded_channel().0,
                    thinking: mpsc::unbounded_channel().0,
                    stats: mpsc::unbounded_channel().0,
                    usage: usage_tx,
                    context: mpsc::unbounded_channel().0,
                },
                mpsc::unbounded_channel().0,
            )
            .await
            .unwrap();
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));

            // The dispatch is the paint path's hand-off: it must return while the quota
            // request still hangs (an awaited fetch would wedge here until the bound).
            let dispatched = tokio::time::timeout(
                Duration::from_millis(500),
                session.handle_slash("/usage", SubmitBehavior::Steer, &mut view),
            )
            .await;
            assert!(
                matches!(dispatched, Ok(Ok(()))),
                "the /usage dispatch returned with the quota half still pending"
            );
            let first = frame_text(&mut view);
            assert!(
                first.contains(&"    Loading provider quotas…".to_string()),
                "{first:#?}"
            );

            // The spend half lands on its own while the quota half still hangs.
            let update = usage_rx.recv().await.expect("the spend half lands");
            session.apply_usage_update(update, &mut view);
            let filled = frame_text(&mut view);
            assert!(
                filled.contains(&"    Loading provider quotas…".to_string())
                    && filled
                        .contains(&"    No billed model calls in local sessions yet.".to_string()),
                "{filled:#?}"
            );
            drop(session);
            server.abort();
        })
        .await
        .expect("the usage page opens and fills within ten seconds");
    }

    /// The tray chip: a rate-limited final assistant frame arms it through the live event path,
    /// it counts down, it clears once the reset passes, and a later success clears it too.
    #[tokio::test]
    async fn the_reset_chip_follows_rate_limits_and_clears() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("chip.sock");
            let server = hanging_quota_daemon(tokio::net::UnixListener::bind(&socket).unwrap());
            let mut options =
                crate::interactive::tests::options(crate::interactive::ModelSelection::default());
            options.socket_path = socket.clone();
            options.session = SessionSelection::Attach("s1".to_string());
            let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
            let mut session = SessionUi::open(
                client,
                &options,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                mpsc::unbounded_channel().0,
                ActivityUpdates {
                    heartbeats: mpsc::unbounded_channel().0,
                    bash: mpsc::unbounded_channel().0,
                    factory: mpsc::unbounded_channel().0,
                    commands: mpsc::unbounded_channel().0,
                    thinking: mpsc::unbounded_channel().0,
                    stats: mpsc::unbounded_channel().0,
                    usage: mpsc::unbounded_channel().0,
                    context: mpsc::unbounded_channel().0,
                },
                mpsc::unbounded_channel().0,
            )
            .await
            .unwrap();
            let mut view = AgentView::new(crate::theme::Theme::builtin(
                "prime",
                crate::theme::ColorMode::TrueColor,
            ));
            let message_end = |message: Value| {
                crate::daemon_client::DaemonClientEvent::SessionEvent {
                    active_session_id: "s1".to_string(),
                    event: json!({"type": "message_end", "message": message}),
                    meta_sequence: 1,
                }
            };
            let limited = json!({
                "role": "assistant", "provider": "anthropic", "model": "m", "content": [],
                "stopReason": "error", "errorMessage": "429 rate limited", "timestamp": 1,
                "diagnostics": [{"type": "provider_stream_failure",
                    "details": {"kind": "rate_limit", "status": 429, "retryAfterMs": 3_720_000}}],
            });
            session.apply_client_event(message_end(limited.clone()), &mut view);
            let now = now_ms();
            session.refresh_reset_chip(&mut view, now + 30_000);
            assert_eq!(view.chrome.reset_chip.as_deref(), Some("limit resets in 1h 1m"));
            session.refresh_reset_chip(&mut view, now + 2_970_000);
            assert_eq!(view.chrome.reset_chip.as_deref(), Some("limit resets in 12m"));
            // The reset passes: the chip clears and stays cleared.
            session.refresh_reset_chip(&mut view, now + 3_800_000);
            assert_eq!(view.chrome.reset_chip, None);
            session.refresh_reset_chip(&mut view, now + 60_000);
            assert_eq!(view.chrome.reset_chip, None);
            // A later success clears an armed chip.
            session.apply_client_event(message_end(limited), &mut view);
            session.refresh_reset_chip(&mut view, now_ms());
            assert!(view.chrome.reset_chip.is_some());
            session.apply_client_event(
                message_end(json!({
                    "role": "assistant", "provider": "anthropic", "model": "m",
                    "content": [{"type": "text", "text": "ok"}], "stopReason": "stop", "timestamp": 2,
                })),
                &mut view,
            );
            session.refresh_reset_chip(&mut view, now_ms());
            assert_eq!(view.chrome.reset_chip, None);
            assert_eq!(session.reset_chips_shown, 2, "each appearance counts once");
            drop(session);
            server.abort();
        })
        .await
        .expect("the chip test completes within ten seconds");
    }
}
