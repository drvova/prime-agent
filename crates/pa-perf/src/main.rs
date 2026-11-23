//! The `pa-perf` CLI: `corpus`, `bench`, `run`, `report`, `session-metrics`, and `trials`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pa_perf::bench::{BenchOptions, RunOptions, Scenario};
use pa_perf::corpus::{self, CacheMode, CorpusOptions, Profile};
use pa_perf::report::{self, Baseline};
use pa_perf::trials::{self, Bootstrap, Prices, SessionTotals};

#[derive(Parser)]
#[command(name = "pa-perf", about = "prime-agent performance tooling", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a deterministic corpus under an isolated agent dir.
    Corpus(CorpusArgs),
    /// Drive the real release binary under a pty and measure one scenario.
    Bench(BenchArgs),
    /// Generate the corpora and run every scenario on the corpus state it
    /// needs (the CI and `make perf` entry point).
    Run(RunArgs),
    /// Compare result JSONs against the baseline; fail on >2x regressions.
    Report(ReportArgs),
    /// Print the per-run harness metrics of a saved session file as JSON.
    SessionMetrics(SessionMetricsArgs),
    /// Aggregate a directory of trial records (cost and time per task and
    /// per pass, with 95% task-bootstrap intervals) as JSON.
    Trials(TrialsArgs),
}

#[derive(clap::Args)]
struct SessionMetricsArgs {
    /// The session JSONL file.
    #[arg(long)]
    session: PathBuf,
}

#[derive(clap::Args)]
struct TrialsArgs {
    /// The directory of `*.json` trial records `{task, attempt, passed,
    /// session_file}` (`session_file` relative to the directory).
    #[arg(long)]
    trials: PathBuf,
    /// The pinned price table: `{input, cached_input, output}` in USD per
    /// million tokens.
    #[arg(long)]
    prices: PathBuf,
    /// A baseline run's trial directory: adds the paired per-task
    /// difference (this run minus the baseline).
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Bootstrap resamples over tasks.
    #[arg(long, default_value_t = 400)]
    resamples: usize,
    /// The bootstrap seed.
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

#[derive(clap::Args)]
struct CorpusArgs {
    /// The named profile providing the default knob values.
    #[arg(long, value_enum, default_value_t = Profile::Small)]
    profile: Profile,
    /// The corpus root: created, then canonicalized (macOS /tmp -> /private/tmp).
    #[arg(long)]
    out: PathBuf,
    /// The deterministic seed.
    #[arg(long, default_value_t = 7)]
    seed: u64,
    /// Top-level sessions in the sessions dir.
    #[arg(long)]
    sessions: Option<usize>,
    /// Rows per session.
    #[arg(long)]
    rows: Option<usize>,
    /// Rows per subagent child session.
    #[arg(long)]
    child_rows: Option<usize>,
    /// Rows of the one huge session.
    #[arg(long)]
    huge_rows: Option<usize>,
    /// Subagent children per parent per level.
    #[arg(long)]
    fanout: Option<usize>,
    /// Subagent tree depth.
    #[arg(long)]
    depth: Option<u32>,
    /// How many sessions grow a subagent tree.
    #[arg(long)]
    tree_parents: Option<usize>,
    /// One parent grows this many direct children (0 disables).
    #[arg(long)]
    wide_children: Option<usize>,
    /// Sessions in sessions-archive/.
    #[arg(long)]
    archive: Option<usize>,
    /// Files in the generated project dir.
    #[arg(long)]
    cwd_files: Option<usize>,
    /// Write the per-session artifact partitions.
    #[arg(long, default_value_t = true)]
    artifacts: bool,
    /// Write the window-cache sidecars via the product's window-store open.
    #[arg(long, value_enum, default_value_t = CacheMode::Cold)]
    cache: CacheMode,
}

#[derive(clap::Args)]
struct BenchArgs {
    /// The release binary under test.
    #[arg(long, default_value = "target/release/prime-agent")]
    binary: PathBuf,
    /// The corpus root (holds corpus.json).
    #[arg(long)]
    corpus: PathBuf,
    /// The scenario to measure.
    #[arg(long, value_enum)]
    scenario: Scenario,
    /// Sampled runs (a warm-up run is always discarded first).
    #[arg(long, default_value_t = 10)]
    runs: usize,
    /// Where the result JSON is written.
    #[arg(long)]
    json: PathBuf,
    /// The per-run liveness deadline (seconds).
    #[arg(long, default_value_t = 180)]
    liveness_secs: u64,
}

#[derive(clap::Args)]
struct RunArgs {
    /// The release binary under test.
    #[arg(long, default_value = "target/release/prime-agent")]
    binary: PathBuf,
    /// The named corpus profile for both generated corpora.
    #[arg(long, value_enum, default_value_t = Profile::Small)]
    profile: Profile,
    /// The output root: the two corpora and results/ land under it.
    #[arg(long, default_value = "target/perf")]
    out: PathBuf,
    /// The deterministic seed.
    #[arg(long, default_value_t = 7)]
    seed: u64,
    /// Sampled runs per scenario (a warm-up run is always discarded first).
    #[arg(long, default_value_t = 10)]
    runs: usize,
    /// The per-run liveness deadline (seconds).
    #[arg(long, default_value_t = 180)]
    liveness_secs: u64,
}

#[derive(clap::Args)]
struct ReportArgs {
    /// One or more bench result JSONs.
    #[arg(long, num_args = 1..)]
    results: Vec<PathBuf>,
    /// The committed baseline file.
    #[arg(long, default_value = "crates/pa-perf/baseline.json")]
    baseline: PathBuf,
    /// Where the markdown table is written (stdout when absent).
    #[arg(long)]
    markdown: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Corpus(args) => corpus_command(args),
        Command::Bench(args) => bench_command(args),
        Command::Run(args) => run_command(args),
        Command::Report(args) => report_command(args),
        Command::SessionMetrics(args) => {
            let runs = pa_core::session_engine::run_metrics::transcript_run_metrics(
                &std::fs::read_to_string(&args.session)?,
            );
            let session = SessionTotals::of(&runs);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "runs": runs,
                    "session": session,
                }))?
            );
            Ok(())
        }
        Command::Trials(args) => {
            let prices: Prices = serde_json::from_str(&std::fs::read_to_string(&args.prices)?)?;
            let options = Bootstrap {
                resamples: args.resamples,
                seed: args.seed,
            };
            let run = trials::load_trials(&args.trials)?;
            let mut output = serde_json::json!({
                "prices": prices,
                "bootstrap": { "resamples": options.resamples, "seed": options.seed },
                "summary": trials::summarize(&run, &prices, options),
            });
            if let Some(baseline) = &args.baseline {
                let baseline = trials::load_trials(baseline)?;
                output["baseline_summary"] =
                    serde_json::to_value(trials::summarize(&baseline, &prices, options))?;
                output["paired"] =
                    serde_json::to_value(trials::paired(&run, &baseline, &prices, options))?;
            }
            println!("{}", serde_json::to_string_pretty(&output)?);
            Ok(())
        }
    }
}

fn corpus_command(args: CorpusArgs) -> anyhow::Result<()> {
    let mut options = CorpusOptions {
        out: args.out.clone(),
        ..args.profile.defaults()
    };
    options.seed = args.seed;
    if let Some(value) = args.sessions {
        options.sessions = value;
    }
    if let Some(value) = args.rows {
        options.rows = value;
    }
    if let Some(value) = args.child_rows {
        options.child_rows = value;
    }
    if let Some(value) = args.huge_rows {
        options.huge_rows = value;
    }
    if let Some(value) = args.fanout {
        options.fanout = value;
    }
    if let Some(value) = args.depth {
        options.depth = value;
    }
    if let Some(value) = args.tree_parents {
        options.tree_parents = value;
    }
    if let Some(value) = args.wide_children {
        options.wide_children = value;
    }
    if let Some(value) = args.archive {
        options.archive = value;
    }
    if let Some(value) = args.cwd_files {
        options.cwd_files = value;
    }
    options.artifacts = args.artifacts;
    options.cache = args.cache;
    let manifest = corpus::generate(&options)?;
    println!(
        "corpus: profile={} sessions={} rows={} total_rows={} child_sessions={} archive={} cache={}",
        report::profile_key(options.profile),
        manifest.counts.sessions,
        options.rows,
        manifest.counts.total_rows,
        manifest.counts.child_sessions,
        manifest.counts.archive,
        report::profile_key_cache(options.cache),
    );
    println!("huge needle: {}", manifest.markers.huge.needle);
    println!("manifest: {}", manifest.paths.manifest_file.display());
    Ok(())
}

fn bench_command(args: BenchArgs) -> anyhow::Result<()> {
    let options = BenchOptions {
        binary: args.binary,
        corpus: args.corpus,
        scenario: args.scenario,
        runs: args.runs,
        json: args.json,
        liveness_secs: args.liveness_secs,
    };
    let result = pa_perf::bench::run(&options)?;
    println!(
        "scenario={} profile={} median={:.1}ms p95={:.1}ms runs={}",
        report::key_of(result.scenario),
        result.profile,
        result.median_ms,
        result.p95_ms,
        result.runs,
    );
    Ok(())
}

fn run_command(args: RunArgs) -> anyhow::Result<()> {
    let options = RunOptions {
        binary: args.binary,
        profile: args.profile,
        out: args.out,
        seed: args.seed,
        runs: args.runs,
        liveness_secs: args.liveness_secs,
    };
    pa_perf::bench::run_suite(&options)?;
    Ok(())
}

fn report_command(args: ReportArgs) -> anyhow::Result<()> {
    let mut results = Vec::new();
    for path in &args.results {
        let result: pa_perf::bench::BenchResult =
            serde_json::from_str(&std::fs::read_to_string(path)?)?;
        results.push(result);
    }
    let baseline: Baseline = serde_json::from_str(&std::fs::read_to_string(&args.baseline)?)?;
    let report = report::report(&results, &baseline);
    match &args.markdown {
        Some(path) => std::fs::write(path, &report.markdown)?,
        None => println!("{}", report.markdown),
    }
    if report.regressions.is_empty() {
        println!("no >2x regressions against the baseline");
        Ok(())
    } else {
        anyhow::bail!("regressions over 2x: {}", report.regressions.join(", "));
    }
}
