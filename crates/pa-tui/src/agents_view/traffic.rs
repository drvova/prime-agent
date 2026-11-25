//! The scoped agents view's Traffic pane: the family's agent messages for the user to watch,
//! display only. The view opts in to the daemon's traffic feed the first time the pane opens;
//! the subscription and the ingest-pause toggle run off the key loop, so a frame never waits on
//! the daemon. Entries live only here (bounded); they never reach a model or a transcript.

use std::collections::{HashMap, HashSet, VecDeque};

use pa_types::agent_traffic::{AgentTrafficEntry, AGENT_TRAFFIC_RING_CAPACITY};
use pa_types::daemon::DaemonCommand;
use tokio::sync::mpsc;

use super::UiInput;
use crate::daemon_client::DaemonClient;
use crate::theme::{Theme, ThemeColor};
use crate::width::{pad_line, str_width};
use crate::Line;

/// The traffic feed's subscription, from the view's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum TrafficSubscription {
    /// The pane never opened: the view holds no subscription.
    #[default]
    Off,
    /// The subscribe request is in flight (the pane says so; the frame does not wait).
    Pending,
    Live,
    /// The subscribe failed; the pane shows the error and the next open retries.
    Failed,
}

/// The Traffic pane's state.
#[derive(Debug, Default)]
pub(super) struct TrafficPane {
    pub(super) shown: bool,
    pub(super) subscription: TrafficSubscription,
    /// The subscribe the run loop dispatches (set by the first open).
    pending_subscribe: bool,
    /// The ingest pause/resume the run loop dispatches: the session and whether to pause.
    pending_pause: Option<(String, bool)>,
    entries: VecDeque<AgentTrafficEntry>,
    /// Unread deliveries per receiving session (live id): counted while the pane is closed.
    unread: HashMap<String, usize>,
    /// Sessions whose message ingest this view paused.
    paused: HashSet<String>,
    error: Option<String>,
}

impl TrafficPane {
    /// The pane key: open (subscribing on the first open, clearing the unread badges) or close.
    pub(super) fn toggle(&mut self) {
        self.shown = !self.shown;
        if self.shown {
            self.unread.clear();
            if matches!(
                self.subscription,
                TrafficSubscription::Off | TrafficSubscription::Failed
            ) {
                self.subscription = TrafficSubscription::Pending;
                self.pending_subscribe = true;
                self.error = None;
            }
        }
    }

    /// The ingest-pause key over one session: pause it, or resume it when this view paused it.
    /// Answers the status line text.
    pub(super) fn toggle_pause(&mut self, active_session_id: &str, name: &str) -> String {
        let pause = !self.paused.contains(active_session_id);
        self.pending_pause = Some((active_session_id.to_string(), pause));
        if pause {
            format!("Pausing agent messages to {name}\u{2026}")
        } else {
            format!("Resuming agent messages to {name}\u{2026}")
        }
    }

    pub(super) fn take_subscribe(&mut self) -> bool {
        std::mem::take(&mut self.pending_subscribe)
    }

    pub(super) fn take_pause(&mut self) -> Option<(String, bool)> {
        self.pending_pause.take()
    }

    /// The landed pause/resume: the view's paused set follows the daemon's answer.
    pub(super) fn pause_settled(&mut self, active_session_id: String, paused: bool) {
        if paused {
            self.paused.insert(active_session_id);
        } else {
            self.paused.remove(&active_session_id);
        }
    }

    /// The landed subscribe: the ring's entries so far (the pushed ones that raced it stay).
    pub(super) fn subscribed(&mut self, result: Result<Vec<AgentTrafficEntry>, String>) {
        match result {
            Ok(snapshot) => {
                self.subscription = TrafficSubscription::Live;
                let pushed: Vec<AgentTrafficEntry> = self.entries.drain(..).collect();
                for entry in snapshot.into_iter().chain(pushed) {
                    self.keep(entry);
                }
            }
            Err(error) => {
                self.subscription = TrafficSubscription::Failed;
                self.error = Some(error);
            }
        }
    }

    /// One pushed entry: kept (bounded), and counted unread while the pane is closed.
    pub(super) fn push(&mut self, entry: AgentTrafficEntry) {
        if !self.shown {
            *self
                .unread
                .entry(entry.to_active_session_id.clone())
                .or_default() += 1;
        }
        self.keep(entry);
    }

    fn keep(&mut self, entry: AgentTrafficEntry) {
        if self.entries.len() >= AGENT_TRAFFIC_RING_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    /// The row's `✉N` badge count.
    pub(super) fn unread_for(&self, active_session_id: &str) -> usize {
        self.unread.get(active_session_id).copied().unwrap_or(0)
    }

    /// The dock counter: every unread delivery.
    pub(super) fn unread_total(&self) -> usize {
        self.unread.values().sum()
    }

    pub(super) fn is_paused(&self, active_session_id: &str) -> bool {
        self.paused.contains(active_session_id)
    }

    /// The pane's rows (a rule header, then the newest entries that fit, newest last).
    pub(super) fn render(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        if rows == 0 {
            return Vec::new();
        }
        let header = match self.subscription {
            TrafficSubscription::Pending => "Traffic \u{b7} connecting\u{2026}".to_string(),
            TrafficSubscription::Failed => format!(
                "Traffic \u{b7} unavailable: {}",
                self.error.as_deref().unwrap_or("unknown error")
            ),
            TrafficSubscription::Off | TrafficSubscription::Live => {
                format!("Traffic \u{b7} {} messages", self.entries.len())
            }
        };
        let rule = width.saturating_sub(str_width(&header) + 4);
        let mut lines = vec![pad_line(
            vec![theme.fg(
                ThemeColor::Dim,
                format!("\u{2500}\u{2500} {header} {}", "\u{2500}".repeat(rule)),
            )],
            width,
        )];
        let body_rows = rows - 1;
        if self.entries.is_empty() {
            if body_rows > 0 {
                lines.push(pad_line(
                    vec![theme.fg(ThemeColor::Dim, "  no agent messages yet".to_string())],
                    width,
                ));
            }
            return lines;
        }
        let skip = self.entries.len().saturating_sub(body_rows);
        for entry in self.entries.iter().skip(skip) {
            lines.push(render_entry(theme, entry, width));
        }
        lines
    }
}

/// One entry: `from → to · relationship · kind outcome · N chars  preview`.
fn render_entry(theme: &Theme, entry: &AgentTrafficEntry, width: usize) -> Line {
    let relationship = entry
        .relationship
        .as_ref()
        .map_or_default(|relationship| format!(" \u{b7} {relationship}"));
    let head = format!("  {} \u{2192} {}{relationship}", entry.from, entry.to);
    let meta = format!(
        " \u{b7} {}{} \u{b7} {} chars  ",
        if entry.kind == "broadcast" {
            "broadcast "
        } else {
            ""
        },
        entry.outcome,
        entry.chars
    );
    let used = str_width(&head) + str_width(&meta);
    let preview =
        crate::agents_view_state::truncate_text(&entry.preview, width.saturating_sub(used));
    let line = vec![
        theme.fg(ThemeColor::Text, head),
        theme.fg(ThemeColor::Dim, meta),
        theme.fg(ThemeColor::Muted, preview),
    ];
    super::render::truncate_line(&pad_line(line, width), width)
}

/// Opt in to the traffic feed off the key loop; the answer lands as `TrafficSubscribed`.
pub(super) fn spawn_traffic_subscribe(
    client: &DaemonClient,
    ui_tx: mpsc::UnboundedSender<UiInput>,
) {
    let client = client.clone();
    tokio::spawn(async move {
        let result = match client
            .request_ok_via_supervisor(DaemonCommand::AgentTrafficSubscribe {
                id: None,
                rest: serde_json::Map::default(),
            })
            .await
        {
            Ok(data) => serde_json::from_value::<Vec<AgentTrafficEntry>>(
                data.get("entries").cloned().unwrap_or_default(),
            )
            .map_err(|error| format!("invalid traffic snapshot: {error}")),
            Err(error) => Err(format!("{error:#}")),
        };
        let _ = ui_tx.send(UiInput::TrafficSubscribed(result));
    });
}

/// Pause or resume one session's agent-message ingest off the key loop.
pub(super) fn spawn_pause_dispatch(
    client: &DaemonClient,
    ui_tx: mpsc::UnboundedSender<UiInput>,
    active_session_id: String,
    pause: bool,
) -> tokio::task::JoinHandle<()> {
    let client = client.clone();
    tokio::spawn(async move {
        let command = if pause {
            DaemonCommand::AgentMessagesPause {
                id: None,
                active_session_id: Some(active_session_id.clone()),
                rest: serde_json::Map::default(),
            }
        } else {
            DaemonCommand::AgentMessagesResume {
                id: None,
                active_session_id: Some(active_session_id.clone()),
                rest: serde_json::Map::default(),
            }
        };
        let outcome = match client.request(command).await {
            Ok(response) if response.success => Ok(response
                .data
                .as_ref()
                .and_then(|data| data.get("paused"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(pause)),
            Ok(response) => Err(response
                .error
                .unwrap_or_else(|| "the command failed".into())),
            Err(error) => Err(format!("{error:#}")),
        };
        let _ = ui_tx.send(UiInput::TrafficPauseResult {
            active_session_id,
            outcome,
        });
    })
}

impl super::AgentsViewMode {
    /// The name of the member a row's session waits on (its summary's `waitingOn`, matched to a
    /// roster row by live or durable id); the raw id when the roster does not list it.
    pub(super) fn waiting_on_name(&self, summary: &serde_json::Value) -> Option<String> {
        let target = summary.get("waitingOn")?.as_str()?;
        let field = |row: &serde_json::Value, key: &str| {
            row.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let name = self.roster.iter().find_map(|entry| {
            let row = entry.get("summary")?;
            let matches = field(row, "activeSessionId").as_deref() == Some(target)
                || field(row, "sessionId").as_deref() == Some(target);
            matches
                .then(|| field(row, "sessionName").filter(|name| !name.is_empty()))
                .flatten()
        });
        Some(name.unwrap_or_else(|| target.to_string()))
    }
}
