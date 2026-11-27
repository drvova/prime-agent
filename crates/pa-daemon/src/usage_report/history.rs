//! The `/usage` page's local spend history: every session file's billed model calls, bucketed by
//! local day and provider/model. A call bills where it ran: each assistant row's own usage block
//! (the raw call, never the child-attributed aggregate, so a parent and its children's files never
//! count one call twice) plus the [`billed_line`] rows (compactions, branch summaries,
//! `model_usage`). The session-stats fold ([`crate::session_usage::UsageScan`]) sums one
//! session's totals without timestamps or models, so it cannot serve a per-day history.
//!
//! Files are cached by length and modification time: a reopened page rescans only the sessions
//! that grew.

use std::collections::{BTreeMap, HashMap};

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use pa_core::session_engine::compaction_exec::add_assistant_usage;
use pa_types::ai::Usage;
use pa_types::billing::{BillingMode, BillingSplit};
use pa_types::provider_usage::{
    DailySpend, ModelSpend, ObservedRateLimit, SpendHistory, SpendTotals,
};
use serde::Deserialize;
use serde_json::Value;

use crate::session_usage::{billed_line, ScanUsage};

/// The history's day span (the spark and the `30 days` total).
pub(crate) const HISTORY_DAYS: i64 = 30;
/// How far back an observed rate limit still shows.
const RATE_LIMIT_HORIZON_MS: i64 = 7 * 86_400_000;
/// Session folders nest one level per working directory; a bound keeps a stray symlink loop out.
const MAX_WALK_DEPTH: usize = 4;
const DAY_MS: i64 = 86_400_000;

/// `(local date, provider, model)`.
type BucketKey = (String, String, String);

/// One file's fold: the billed usage, call count and billing split per bucket (ordered, so the
/// cost sums add in one order every run), the latest rate-limited response per provider, and each
/// model's latest assistant call (its time and billing mode).
#[derive(Debug, Clone, Default, PartialEq)]
struct FileSpend {
    buckets: BTreeMap<BucketKey, (Usage, u64, BillingSplit)>,
    rate_limits: HashMap<String, ObservedRateLimit>,
    latest_modes: HashMap<(String, String), (i64, Option<BillingMode>)>,
}

/// The scan's file identity: a changed length or mtime rescans the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

fn file_cache() -> &'static Mutex<HashMap<PathBuf, (FileStamp, FileSpend)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (FileStamp, FileSpend)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanLine {
    #[serde(rename = "type", default)]
    type_: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    message: Option<ScanMessage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<ScanUsage>,
    #[serde(default)]
    timestamp: Option<f64>,
    #[serde(default)]
    diagnostics: Option<Vec<Value>>,
}

/// The local date (`YYYY-MM-DD`) of `epoch_ms`; UTC where the platform has no zone binding.
fn date_key(epoch_ms: i64) -> String {
    match pa_types::platform::local_civil(epoch_ms) {
        Some(civil) => format!("{:04}-{:02}-{:02}", civil.year, civil.month, civil.day),
        None => crate::util::iso_from_unix_ms(u64::try_from(epoch_ms).unwrap_or_default())
            .chars()
            .take(10)
            .collect(),
    }
}

/// The last [`HISTORY_DAYS`] local dates, oldest first: stepped from today's local noon, so a
/// daylight-saving shift never skips or repeats a date.
fn recent_dates(now_ms: i64) -> Vec<String> {
    let noon = pa_types::platform::local_civil(now_ms).map_or(now_ms, |civil| {
        now_ms - (i64::from(civil.hour) - 12) * 3_600_000 - i64::from(civil.minute) * 60_000
    });
    (0..HISTORY_DAYS)
        .rev()
        .map(|back| date_key(noon - back * DAY_MS))
        .collect()
}

/// The provider-reported reset of a rate-limited assistant row: its `provider_stream_failure`
/// diagnostic with HTTP 429 (or the `rate_limit` kind), reset at `timestamp + retryAfterMs`.
fn observed_rate_limit(message: &ScanMessage, at_ms: i64) -> Option<ObservedRateLimit> {
    let details = message
        .diagnostics
        .as_ref()?
        .iter()
        .find_map(|diagnostic| {
            (diagnostic.get("type").and_then(Value::as_str) == Some("provider_stream_failure"))
                .then(|| diagnostic.get("details"))
                .flatten()
        })?;
    let rate_limited = details.get("status").and_then(Value::as_u64) == Some(429)
        || details.get("kind").and_then(Value::as_str) == Some("rate_limit");
    if !rate_limited {
        return None;
    }
    Some(ObservedRateLimit {
        provider: message.provider.clone()?,
        observed_at_ms: at_ms,
        resets_at_ms: details
            .get("retryAfterMs")
            .and_then(Value::as_i64)
            .map(|wait| at_ms + wait),
    })
}

/// Fold one session file. Unreadable lines are skipped (a torn tail never fails the page).
fn scan_file(path: &Path) -> FileSpend {
    let mut spend = FileSpend::default();
    let Ok(file) = pa_core::session::store::store().open(path) else {
        return spend;
    };
    // A later write of one assistant row replaces the earlier one in place (the session fold's
    // rule); file order is kept.
    let mut assistant_rows: Vec<(BucketKey, Usage)> = Vec::new();
    let mut assistant_index: HashMap<String, usize> = HashMap::new();
    let mut billed_rows: Vec<(BucketKey, Usage)> = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<ScanLine>(&line) else {
            continue;
        };
        let entry_ms = entry
            .timestamp
            .as_deref()
            .and_then(crate::util::iso_to_unix_ms)
            .and_then(|ms| i64::try_from(ms).ok());
        if let Some(row) = billed_line(&entry.type_, &line) {
            let (provider, model) = row
                .model
                .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));
            if let Some(at_ms) = entry_ms {
                billed_rows.push(((date_key(at_ms), provider, model), row.usage));
            }
            continue;
        }
        let Some(message) = entry.message.filter(|_| entry.type_ == "message") else {
            continue;
        };
        if message.role.as_deref() != Some("assistant") {
            continue;
        }
        let Some(at_ms) = message.timestamp.map(|ms| ms as i64).or(entry_ms) else {
            continue;
        };
        if let Some(observed) = observed_rate_limit(&message, at_ms) {
            let newer = spend
                .rate_limits
                .get(&observed.provider)
                .is_none_or(|known| known.observed_at_ms <= at_ms);
            if newer {
                spend
                    .rate_limits
                    .insert(observed.provider.clone(), observed);
            }
        }
        if let Some(usage) = message.usage {
            let key = (
                date_key(at_ms),
                message.provider.unwrap_or_else(|| "unknown".to_string()),
                message.model.unwrap_or_else(|| "unknown".to_string()),
            );
            let usage: Usage = usage.into();
            let latest = spend
                .latest_modes
                .entry((key.1.clone(), key.2.clone()))
                .or_insert((at_ms, usage.billing));
            if latest.0 <= at_ms {
                *latest = (at_ms, usage.billing);
            }
            if let Some(at) = assistant_index.get(&entry.id) {
                assistant_rows[*at] = (key, usage);
            } else {
                assistant_index.insert(entry.id, assistant_rows.len());
                assistant_rows.push((key, usage));
            }
        }
    }
    for (key, usage) in assistant_rows.into_iter().chain(billed_rows) {
        let billed = usage.input + usage.output + usage.cache_read + usage.cache_write > 0
            || usage.cost.total.as_f64() > 0.0;
        if !billed {
            continue;
        }
        let bucket = spend.buckets.entry(key).or_default();
        add_assistant_usage(&mut bucket.0, &usage);
        bucket.1 += 1;
        bucket
            .2
            .add(usage.billing.as_ref(), usage.cost.total.as_f64());
    }
    spend
}

/// Every `.jsonl` under the sessions root, depth-bounded.
fn session_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(listing) = pa_core::session::store::store().list(&dir) else {
            continue;
        };
        if depth < MAX_WALK_DEPTH {
            pending.extend(listing.directories.into_iter().map(|dir| (dir, depth + 1)));
        }
        files.extend(
            listing
                .transcripts
                .into_iter()
                .filter(|transcript| transcript.listed_as_transcript)
                .map(|transcript| transcript.locator),
        );
    }
    files.sort();
    files
}

fn add_totals(totals: &mut SpendTotals, (usage, requests, billing): &(Usage, u64, BillingSplit)) {
    totals.input_tokens += usage.input;
    totals.output_tokens += usage.output;
    totals.cache_read_tokens += usage.cache_read;
    totals.cache_write_tokens += usage.cache_write;
    totals.cost += usage.cost.total.as_f64();
    totals.requests += requests;
    totals.billing.merge(billing);
}

/// Scan the sessions root (cached per file) and aggregate the page's spans at `now_ms`.
/// Blocking file IO: the caller runs it off the async runtime.
pub(crate) fn spend_history(sessions_root: &Path, now_ms: i64) -> SpendHistory {
    let files = session_files(sessions_root);
    let mut folds: Vec<FileSpend> = Vec::with_capacity(files.len());
    for path in &files {
        let stamp = pa_core::session::store::store().stat(path).map_or(
            FileStamp {
                len: 0,
                modified: None,
            },
            |stat| FileStamp {
                len: stat.len(),
                modified: stat.modified,
            },
        );
        let cached = file_cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .filter(|(known, _)| *known == stamp)
            .map(|(_, spend)| spend.clone());
        let spend = cached.unwrap_or_else(|| {
            let spend = scan_file(path);
            file_cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.clone(), (stamp, spend.clone()));
            spend
        });
        folds.push(spend);
    }

    let dates = recent_dates(now_ms);
    let today = dates.last().cloned().unwrap_or_default();
    let week_start = &dates[dates.len().saturating_sub(7)];
    let month_start = &dates[0];
    let mut daily: BTreeMap<&str, SpendTotals> = dates
        .iter()
        .map(|date| (date.as_str(), SpendTotals::default()))
        .collect();
    let mut history = SpendHistory {
        today: SpendTotals::default(),
        last_7_days: SpendTotals::default(),
        last_30_days: SpendTotals::default(),
        all_time: SpendTotals::default(),
        daily: Vec::new(),
        models: Vec::new(),
        rate_limits: Vec::new(),
        sessions: files.len() as u64,
    };
    let mut models: BTreeMap<(String, String), ModelSpend> = BTreeMap::new();
    let mut rate_limits: BTreeMap<String, ObservedRateLimit> = BTreeMap::new();
    let mut latest_modes: HashMap<(String, String), (i64, Option<BillingMode>)> = HashMap::new();
    for fold in &folds {
        for (model, latest) in &fold.latest_modes {
            let known = latest_modes.entry(model.clone()).or_insert(*latest);
            if known.0 <= latest.0 {
                *known = *latest;
            }
        }
        for ((date, provider, model), bucket) in &fold.buckets {
            let row = models
                .entry((provider.clone(), model.clone()))
                .or_insert_with(|| ModelSpend {
                    provider: provider.clone(),
                    model: model.clone(),
                    mode: None,
                    today: SpendTotals::default(),
                    last_7_days: SpendTotals::default(),
                    last_30_days: SpendTotals::default(),
                    all_time: SpendTotals::default(),
                });
            // Dates compare as `YYYY-MM-DD` strings; a future-dated row (clock skew) counts
            // in every span it is not older than.
            let spans = [
                (*date == today, &mut history.today, &mut row.today),
                (
                    date.as_str() >= week_start.as_str(),
                    &mut history.last_7_days,
                    &mut row.last_7_days,
                ),
                (
                    date.as_str() >= month_start.as_str(),
                    &mut history.last_30_days,
                    &mut row.last_30_days,
                ),
                (true, &mut history.all_time, &mut row.all_time),
            ];
            for (inside, total, model_total) in spans {
                if inside {
                    add_totals(total, bucket);
                    add_totals(model_total, bucket);
                }
            }
            if let Some(day) = daily.get_mut(date.as_str()) {
                add_totals(day, bucket);
            }
        }
        for observed in fold.rate_limits.values() {
            if now_ms - observed.observed_at_ms > RATE_LIMIT_HORIZON_MS {
                continue;
            }
            let newer = rate_limits
                .get(&observed.provider)
                .is_none_or(|known| known.observed_at_ms <= observed.observed_at_ms);
            if newer {
                rate_limits.insert(observed.provider.clone(), observed.clone());
            }
        }
    }
    history.daily = daily
        .into_iter()
        .map(|(date, totals)| DailySpend {
            date: date.to_string(),
            totals,
        })
        .collect();
    history.models = models
        .into_values()
        .map(|row| ModelSpend {
            mode: latest_modes
                .get(&(row.provider.clone(), row.model.clone()))
                .and_then(|(_, mode)| *mode),
            ..row
        })
        .collect();
    history.models.sort_by(|left, right| {
        right
            .all_time
            .cost
            .total_cmp(&left.all_time.cost)
            .then_with(|| {
                right
                    .all_time
                    .total_tokens()
                    .cmp(&left.all_time.total_tokens())
            })
    });
    history.rate_limits = rate_limits.into_values().collect();
    history
}
