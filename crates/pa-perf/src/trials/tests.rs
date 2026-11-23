//! The aggregation against a small hand-computed example. With two tasks a
//! resample is one of four task pairs, each drawn about 100 times in 400,
//! so the 2.5% and 97.5% points are the extreme pairs' values: the
//! intervals below follow by hand, and the fixed seed pins the draw.

use super::*;

const PRICES: Prices = Prices {
    input: 1.0,
    cached_input: 0.5,
    output: 2.0,
};

const BOOTSTRAP: Bootstrap = Bootstrap {
    resamples: 400,
    seed: 11,
};

/// A trial whose tokens are `[uncached, cache read, cache write, output]`
/// (at [`PRICES`], a million uncached tokens cost $1).
fn trial(
    task: &str,
    passed: bool,
    tokens: [u64; 4],
    wall_ms: u64,
    steps: u64,
    end_reason: TurnEndReason,
) -> Trial {
    let [uncached_input, cache_read, cache_write, output] = tokens;
    Trial {
        task: task.to_string(),
        passed,
        totals: SessionTotals {
            runs: 1,
            model_requests: steps,
            eval_cells: 0,
            tool_errors: 0,
            tokens: TokenSplit {
                uncached_input,
                cache_read,
                cache_write,
                output,
            },
            cache_breaks: None,
            compactions: 0,
            wall_ms,
            end_reason,
        },
    }
}

const M: u64 = 1_000_000;

/// t1: one pass of two, $1 each, 100 ms and 300 ms, 2 and 4 steps.
/// t2: no pass, $2 each, 200 ms and 400 ms, 6 and 8 steps.
fn baseline_run() -> Vec<Trial> {
    vec![
        trial("t1", true, [M, 0, 0, 0], 100, 2, TurnEndReason::Stop),
        trial("t1", false, [0, 2 * M, 0, 0], 300, 4, TurnEndReason::Stop),
        trial("t2", false, [0, 0, 0, M], 200, 6, TurnEndReason::Error),
        trial("t2", false, [M, 0, M, 0], 400, 8, TurnEndReason::Stop),
    ]
}

fn estimate(value: f64, low: f64, high: f64) -> Estimate {
    Estimate {
        value: Some(value),
        low: Some(low),
        high: Some(high),
    }
}

/// Pass rate 1/4; cost per task (1 + 2) / 2 = 1.5, per pass 1.5 / 0.25 = 6;
/// time per task (200 + 300) / 2 = 250, per pass 1,000; steps (3 + 7) / 2.
/// The resample t2,t2 has no pass, so the per-pass intervals run from the
/// t1,t1 value (1 / 0.5, 200 / 0.5) to the mixed one.
#[test]
fn a_run_aggregates_per_task_then_over_tasks() {
    assert_eq!(
        summarize(&baseline_run(), &PRICES, BOOTSTRAP),
        Summary {
            tasks: 2,
            trials: 4,
            pass_rate: estimate(0.25, 0.0, 0.5),
            cost_per_task_usd: estimate(1.5, 1.0, 2.0),
            cost_per_pass_usd: estimate(6.0, 2.0, 6.0),
            time_per_task_ms: estimate(250.0, 200.0, 300.0),
            time_per_pass_ms: estimate(1_000.0, 400.0, 1_000.0),
            steps_per_task: estimate(5.0, 3.0, 7.0),
            steps_per_passed_trial: Some(2.0),
            failed_cost_share: Some(5.0 / 6.0),
            failed_final_answer_share: Some(2.0 / 3.0),
            per_task: vec![
                TaskMeans {
                    task: "t1".to_string(),
                    attempts: 2,
                    passes: 1,
                    cost_usd: 1.0,
                    time_ms: 200.0,
                    steps: 3.0,
                },
                TaskMeans {
                    task: "t2".to_string(),
                    attempts: 2,
                    passes: 0,
                    cost_usd: 2.0,
                    time_ms: 300.0,
                    steps: 7.0,
                },
            ],
        }
    );
}

/// Against the baseline: t1 passes +0.5 at -$0.5 and -100 ms, t2 passes
/// +0.5 at -$1 and the same time; t3 ran only in the new run and is not
/// compared.
#[test]
fn two_runs_compare_per_task_paired() {
    let run = vec![
        trial("t1", true, [M / 2, 0, 0, 0], 100, 1, TurnEndReason::Stop),
        trial("t1", true, [M / 2, 0, 0, 0], 100, 1, TurnEndReason::Stop),
        trial("t2", true, [M, 0, 0, 0], 300, 1, TurnEndReason::Stop),
        trial("t2", false, [M, 0, 0, 0], 300, 1, TurnEndReason::Stop),
        trial("t3", true, [M, 0, 0, 0], 300, 1, TurnEndReason::Stop),
    ];
    assert_eq!(
        paired(&run, &baseline_run(), &PRICES, BOOTSTRAP),
        Paired {
            tasks_compared: 2,
            pass_rate: estimate(0.5, 0.5, 0.5),
            cost_usd: estimate(-0.75, -1.0, -0.5),
            time_ms: estimate(-50.0, -100.0, 0.0),
        }
    );
}
