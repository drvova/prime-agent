//! Harness-eval aggregation over trial records, the FrontierHarness Eval
//! method: a task's cost, time and steps are the mean over its attempts,
//! a run's are the mean over tasks, a pass costs the per-task value divided
//! by the pooled pass rate, and every estimate carries a 95% bootstrap over
//! tasks. Two runs compare per task, paired.
//!
//! A trial is one attempt at one task: a record `{task, attempt, passed,
//! session_file}` beside the saved session it produced. Its metrics come
//! from that session file through pa-core's
//! `session_engine::run_metrics::transcript_run_metrics`, the same
//! accounting the live `agent run completed` event carries. Cost is the
//! trial's tokens at a pinned price table, never a provider's bill: cache
//! writes are priced as uncached input (the table has no write rate).
//! Time is the agent's wall time from the session file (verifier time is
//! not in it).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context as _;
use pa_core::session_engine::run_metrics::{
    transcript_run_metrics, RunMetrics, TokenSplit, TurnEndReason,
};
use serde::{Deserialize, Serialize};

use crate::rng::Rng;

#[cfg(test)]
mod tests;

/// One trial record file.
#[derive(Debug, Deserialize)]
struct TrialRecord {
    task: String,
    attempt: u64,
    passed: bool,
    /// The saved session, relative to the record's directory.
    session_file: String,
}

/// List prices in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Prices {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

/// The bootstrap's resample count and seed.
#[derive(Debug, Clone, Copy)]
pub struct Bootstrap {
    pub resamples: usize,
    pub seed: u64,
}

/// One session's runs folded together.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionTotals {
    pub runs: u64,
    pub model_requests: u64,
    pub eval_cells: u64,
    pub tool_errors: u64,
    pub tokens: TokenSplit,
    /// The runs' breaks summed; `None` when no run could compare.
    pub cache_breaks: Option<u64>,
    pub compactions: u64,
    pub wall_ms: u64,
    /// How the last run ended.
    pub end_reason: TurnEndReason,
}

impl SessionTotals {
    #[must_use]
    pub fn of(runs: &[RunMetrics]) -> Self {
        let mut totals = Self {
            runs: runs.len() as u64,
            model_requests: 0,
            eval_cells: 0,
            tool_errors: 0,
            tokens: TokenSplit::default(),
            cache_breaks: None,
            compactions: 0,
            wall_ms: 0,
            end_reason: runs
                .last()
                .map_or(TurnEndReason::Unknown, |run| run.end_reason),
        };
        for run in runs {
            totals.model_requests += run.model_requests;
            totals.eval_cells += run.eval_cells;
            totals.tool_errors += run.tool_errors;
            totals.tokens.uncached_input += run.tokens.uncached_input;
            totals.tokens.cache_read += run.tokens.cache_read;
            totals.tokens.cache_write += run.tokens.cache_write;
            totals.tokens.output += run.tokens.output;
            if let Some(breaks) = run.cache_breaks {
                totals.cache_breaks = Some(totals.cache_breaks.unwrap_or(0) + breaks);
            }
            totals.compactions += run.compactions.len() as u64;
            totals.wall_ms += run.wall_ms;
        }
        totals
    }
}

/// One scored attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    pub task: String,
    pub passed: bool,
    pub totals: SessionTotals,
}

/// Read every `*.json` trial record in `dir` and the session file each
/// names.
///
/// # Errors
///
/// An unreadable directory, record or session file, a malformed record, or
/// two records for the same task and attempt.
pub fn load_trials(dir: &Path) -> anyhow::Result<Vec<Trial>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading trial records in {}", dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    paths.sort();
    let mut seen = std::collections::BTreeSet::new();
    let mut trials = Vec::with_capacity(paths.len());
    for path in paths {
        let record: TrialRecord = serde_json::from_str(
            &std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing trial record {}", path.display()))?;
        anyhow::ensure!(
            seen.insert((record.task.clone(), record.attempt)),
            "{}: task {} attempt {} is recorded twice",
            path.display(),
            record.task,
            record.attempt
        );
        let session = dir.join(&record.session_file);
        let content = std::fs::read_to_string(&session)
            .with_context(|| format!("reading session file {}", session.display()))?;
        trials.push(Trial {
            task: record.task,
            passed: record.passed,
            totals: SessionTotals::of(&transcript_run_metrics(&content)),
        });
    }
    Ok(trials)
}

/// A point estimate with its 95% bootstrap interval; `None` where the
/// statistic is undefined (a per-pass value without passes).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Estimate {
    pub value: Option<f64>,
    pub low: Option<f64>,
    pub high: Option<f64>,
}

/// One task's attempts, averaged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskMeans {
    pub task: String,
    pub attempts: u64,
    pub passes: u64,
    pub cost_usd: f64,
    pub time_ms: f64,
    pub steps: f64,
}

/// A run of trials, aggregated.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub tasks: u64,
    pub trials: u64,
    /// Passes over scored attempts.
    pub pass_rate: Estimate,
    pub cost_per_task_usd: Estimate,
    pub cost_per_pass_usd: Estimate,
    pub time_per_task_ms: Estimate,
    pub time_per_pass_ms: Estimate,
    /// Model requests per task.
    pub steps_per_task: Estimate,
    /// Model requests of a passed trial, the mean over passed trials.
    pub steps_per_passed_trial: Option<f64>,
    /// The share of all cost spent on trials that did not pass.
    pub failed_cost_share: Option<f64>,
    /// The share of failed trials whose last run ended in a final answer
    /// (`stop` or `empty`) rather than an error, abort or cut.
    pub failed_final_answer_share: Option<f64>,
    pub per_task: Vec<TaskMeans>,
}

/// The paired per-task difference `run - baseline` over the tasks both ran.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Paired {
    pub tasks_compared: u64,
    pub pass_rate: Estimate,
    pub cost_usd: Estimate,
    pub time_ms: Estimate,
}

/// A trial's cost at `prices`.
// Token counts stay far below 2^53.
#[allow(clippy::cast_precision_loss)]
fn trial_cost(tokens: &TokenSplit, prices: &Prices) -> f64 {
    ((tokens.uncached_input + tokens.cache_write) as f64 * prices.input
        + tokens.cache_read as f64 * prices.cached_input
        + tokens.output as f64 * prices.output)
        / 1_000_000.0
}

/// Each task's means, in task order.
// Counts stay far below 2^53.
#[allow(clippy::cast_precision_loss)]
fn task_means(trials: &[Trial], prices: &Prices) -> Vec<TaskMeans> {
    let mut by_task: BTreeMap<&str, Vec<&Trial>> = BTreeMap::new();
    for trial in trials {
        by_task.entry(trial.task.as_str()).or_default().push(trial);
    }
    by_task
        .into_iter()
        .map(|(task, attempts)| {
            let count = attempts.len() as f64;
            let mean = |value: &dyn Fn(&Trial) -> f64| {
                attempts.iter().map(|trial| value(trial)).sum::<f64>() / count
            };
            TaskMeans {
                task: task.to_string(),
                attempts: attempts.len() as u64,
                passes: attempts.iter().filter(|trial| trial.passed).count() as u64,
                cost_usd: mean(&|trial: &Trial| trial_cost(&trial.totals.tokens, prices)),
                time_ms: mean(&|trial: &Trial| trial.totals.wall_ms as f64),
                steps: mean(&|trial: &Trial| trial.totals.model_requests as f64),
            }
        })
        .collect()
}

/// `[pass rate, cost/task, cost/pass, time/task, time/pass, steps/task]`
/// over the tasks at `picks` (a resample may pick a task twice).
// Counts stay far below 2^53.
#[allow(clippy::cast_precision_loss)]
fn run_statistics(tasks: &[TaskMeans], picks: &[usize]) -> [Option<f64>; 6] {
    let count = picks.len() as f64;
    let mean = |value: &dyn Fn(&TaskMeans) -> f64| {
        picks.iter().map(|pick| value(&tasks[*pick])).sum::<f64>() / count
    };
    let attempts: u64 = picks.iter().map(|pick| tasks[*pick].attempts).sum();
    let passes: u64 = picks.iter().map(|pick| tasks[*pick].passes).sum();
    let pass_rate = passes as f64 / attempts as f64;
    let cost = mean(&|task: &TaskMeans| task.cost_usd);
    let time = mean(&|task: &TaskMeans| task.time_ms);
    let per_pass = |value: f64| (passes > 0).then(|| value / pass_rate);
    [
        Some(pass_rate),
        Some(cost),
        per_pass(cost),
        Some(time),
        per_pass(time),
        Some(mean(&|task: &TaskMeans| task.steps)),
    ]
}

/// The 95% percentile interval of `K` statistics over `resamples` draws of
/// `n` items with replacement; a draw where a statistic is undefined is
/// left out of that statistic's interval.
fn bootstrap<const K: usize>(
    n: usize,
    options: Bootstrap,
    statistic: impl Fn(&[usize]) -> [Option<f64>; K],
) -> [(Option<f64>, Option<f64>); K] {
    if n == 0 {
        return [(None, None); K];
    }
    let mut rng = Rng::new(options.seed);
    let mut draws: [Vec<f64>; K] = std::array::from_fn(|_| Vec::with_capacity(options.resamples));
    let mut picks = vec![0; n];
    for _ in 0..options.resamples {
        for pick in &mut picks {
            *pick = usize::try_from(rng.below(n as u64)).expect("a task index fits usize");
        }
        for (values, value) in draws.iter_mut().zip(statistic(&picks)) {
            values.extend(value);
        }
    }
    draws.map(|mut values| {
        values.sort_by(f64::total_cmp);
        // Lengths stay far below 2^53.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let at = |quantile: f64| {
            let last = values.len().checked_sub(1)?;
            values
                .get((last as f64 * quantile).round() as usize)
                .copied()
        };
        (at(0.025), at(0.975))
    })
}

/// Aggregate one run of trials.
#[must_use]
// Counts stay far below 2^53.
#[allow(clippy::cast_precision_loss)]
pub fn summarize(trials: &[Trial], prices: &Prices, options: Bootstrap) -> Summary {
    let per_task = task_means(trials, prices);
    let all: Vec<usize> = (0..per_task.len()).collect();
    let point = if per_task.is_empty() {
        [None; 6]
    } else {
        run_statistics(&per_task, &all)
    };
    let intervals = bootstrap(per_task.len(), options, |picks| {
        run_statistics(&per_task, picks)
    });
    let [pass_rate, cost_per_task_usd, cost_per_pass_usd, time_per_task_ms, time_per_pass_ms, steps_per_task] =
        std::array::from_fn(|index| Estimate {
            value: point[index],
            low: intervals[index].0,
            high: intervals[index].1,
        });
    let share = |part: f64, whole: f64| (whole > 0.0).then(|| part / whole);
    let passed: Vec<&Trial> = trials.iter().filter(|trial| trial.passed).collect();
    let failed: Vec<&Trial> = trials.iter().filter(|trial| !trial.passed).collect();
    let cost = |trials: &[&Trial]| {
        trials
            .iter()
            .map(|trial| trial_cost(&trial.totals.tokens, prices))
            .sum::<f64>()
    };
    let final_answers = failed
        .iter()
        .filter(|trial| match trial.totals.end_reason {
            TurnEndReason::Stop | TurnEndReason::Empty => true,
            TurnEndReason::Length
            | TurnEndReason::ToolUse
            | TurnEndReason::Error
            | TurnEndReason::Aborted
            | TurnEndReason::Unknown
            | TurnEndReason::LoopGuard => false,
        })
        .count();
    Summary {
        tasks: per_task.len() as u64,
        trials: trials.len() as u64,
        pass_rate,
        cost_per_task_usd,
        cost_per_pass_usd,
        time_per_task_ms,
        time_per_pass_ms,
        steps_per_task,
        steps_per_passed_trial: share(
            passed
                .iter()
                .map(|trial| trial.totals.model_requests as f64)
                .sum(),
            passed.len() as f64,
        ),
        failed_cost_share: share(cost(&failed), cost(&failed) + cost(&passed)),
        failed_final_answer_share: share(final_answers as f64, failed.len() as f64),
        per_task,
    }
}

/// Compare `run` against `baseline` per task, over the tasks both ran.
#[must_use]
// Counts stay far below 2^53.
#[allow(clippy::cast_precision_loss)]
pub fn paired(run: &[Trial], baseline: &[Trial], prices: &Prices, options: Bootstrap) -> Paired {
    let baseline: BTreeMap<String, TaskMeans> = task_means(baseline, prices)
        .into_iter()
        .map(|task| (task.task.clone(), task))
        .collect();
    let rate = |task: &TaskMeans| task.passes as f64 / task.attempts as f64;
    let differences: Vec<[f64; 3]> = task_means(run, prices)
        .iter()
        .filter_map(|task| {
            let before = baseline.get(&task.task)?;
            Some([
                rate(task) - rate(before),
                task.cost_usd - before.cost_usd,
                task.time_ms - before.time_ms,
            ])
        })
        .collect();
    let mean = |picks: &[usize]| -> [Option<f64>; 3] {
        std::array::from_fn(|index| {
            (!picks.is_empty()).then(|| {
                picks
                    .iter()
                    .map(|pick| differences[*pick][index])
                    .sum::<f64>()
                    / picks.len() as f64
            })
        })
    };
    let all: Vec<usize> = (0..differences.len()).collect();
    let point = mean(&all);
    let intervals = bootstrap(differences.len(), options, mean);
    let [pass_rate, cost_usd, time_ms] = std::array::from_fn(|index| Estimate {
        value: point[index],
        low: intervals[index].0,
        high: intervals[index].1,
    });
    Paired {
        tasks_compared: differences.len() as u64,
        pass_rate,
        cost_usd,
        time_ms,
    }
}
