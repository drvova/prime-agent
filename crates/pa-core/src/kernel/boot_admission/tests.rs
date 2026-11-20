use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

use pa_ai::admission::{GrantFuture, LocalAdmission};
use pa_types::control::{AdjustReason, Controller, LearnedState, Target};

use super::*;

const MEMINFO: &str =
    "MemTotal:       16000000 kB\nMemFree:          100000 kB\nMemAvailable:    4000000 kB\n";

#[test]
fn host_load_reads_proc_text_and_pressure_overloads_a_boot() {
    let calm = HostLoad::parse("1.50 1.20 1.00 2/300 4000\n", MEMINFO, 4);
    let busy = HostLoad::parse("9.2 8.0 7.0 9/300 4000\n", MEMINFO, 4);
    let short_on_memory = HostLoad::parse(
        "0.10 0.10 0.10 1/300 4000\n",
        "MemTotal: 1000 kB\nMemAvailable: 50 kB\n",
        4,
    );
    assert_eq!(
        (calm, busy, short_on_memory),
        (
            Some(HostLoad {
                load_per_core_pct: 37,
                memory_available_pct: 25,
            }),
            Some(HostLoad {
                load_per_core_pct: 230,
                memory_available_pct: 25,
            }),
            Some(HostLoad {
                load_per_core_pct: 2,
                memory_available_pct: 5,
            }),
        )
    );
    assert_eq!(
        [
            boot_outcome(BootResult::Booted, calm),
            boot_outcome(BootResult::Booted, busy),
            boot_outcome(BootResult::Failed, short_on_memory),
            boot_outcome(BootResult::Failed, calm),
            boot_outcome(BootResult::Booted, None),
        ],
        [
            Outcome::Success,
            Outcome::Overloaded,
            Outcome::Overloaded,
            Outcome::Failed,
            Outcome::Success,
        ]
    );
}

#[test]
fn the_boot_width_climbs_while_boots_are_fast_and_halves_under_pressure() {
    let config = pa_ai::admission::session_boot_controller_config();
    let start = LearnedState {
        concurrency: 4,
        rate_per_min: 100_000,
        concurrency_ceiling: None,
        rate_ceiling: None,
        ceiling_set_at_ms: None,
        reason: AdjustReason::Restored,
        updated_at_ms: 0,
    };
    let mut controller = Controller::restore(config, start, 0);
    let calm = HostLoad {
        load_per_core_pct: 50,
        memory_available_pct: 60,
    };
    let pressure = HostLoad {
        load_per_core_pct: 50,
        memory_available_pct: 4,
    };
    // Four fast boots earn a step; a boot under memory pressure halves the width; a doubled
    // boot latency twice in a row backs it off.
    let boots = [
        (calm, 800),
        (calm, 820),
        (calm, 790),
        (calm, 810),
        (pressure, 900),
        (calm, 2_500),
        (calm, 2_600),
    ];
    let targets: Vec<(u32, AdjustReason)> = boots
        .iter()
        .enumerate()
        .map(|(at, (load, latency_ms))| {
            let outcome = boot_outcome(BootResult::Booted, Some(*load));
            let Target {
                concurrency,
                reason,
                ..
            } = controller.observe(
                &Observation {
                    outcome,
                    latency_ms: (outcome == Outcome::Success).then_some(*latency_ms),
                    limits: LimitReport::default(),
                },
                (u64::try_from(at).unwrap() + 1) * 10_000,
            );
            (concurrency, reason)
        })
        .collect();
    assert_eq!(
        targets,
        [
            (4, AdjustReason::Restored),
            (4, AdjustReason::Restored),
            (4, AdjustReason::Restored),
            (5, AdjustReason::Headroom),
            (2, AdjustReason::Overloaded),
            (2, AdjustReason::Overloaded),
            (1, AdjustReason::LatencyRise),
        ]
    );
}

#[test]
fn the_kernel_boot_override_pins_a_hard_width() {
    assert_eq!(
        [
            pa_ai::admission::pinned_boot_concurrency(Some("2")),
            pa_ai::admission::pinned_boot_concurrency(Some("1000")),
            pa_ai::admission::pinned_boot_concurrency(Some("0")),
            pa_ai::admission::pinned_boot_concurrency(Some("junk")),
            pa_ai::admission::pinned_boot_concurrency(Some("")),
            pa_ai::admission::pinned_boot_concurrency(None),
        ],
        [Some(2), Some(64), None, None, None, None]
    );
}

/// A pool whose session-boot width starts at one.
fn single_boot_pool() -> Arc<dyn AdmissionSource> {
    let pool = LocalAdmission::new(pa_ai::admission::provider_controller_config());
    pool.seed([(
        AdmissionKey::session_boot(),
        LearnedState {
            concurrency: 1,
            rate_per_min: 100_000,
            concurrency_ceiling: Some(1),
            rate_ceiling: None,
            ceiling_set_at_ms: Some(pa_ai::admission::now_ms()),
            reason: AdjustReason::Restored,
            updated_at_ms: 0,
        },
    )]);
    Arc::new(pool)
}

#[tokio::test]
async fn the_foreground_boots_first_and_a_waiter_that_gives_up_holds_no_slot() {
    let source = single_boot_pool();
    let order = Arc::new(Mutex::new(Vec::new()));
    let boot = |label: &'static str, release: Option<tokio::sync::oneshot::Receiver<()>>| {
        let order = Arc::clone(&order);
        async move {
            order.lock().unwrap().push(label);
            if let Some(release) = release {
                let _ = release.await;
            }
            anyhow::Ok(label)
        }
    };
    let (release_first, held) = tokio::sync::oneshot::channel();
    let first = tokio::spawn(admitted_boot(
        Arc::clone(&source),
        Priority::Subagent,
        boot("first", Some(held)),
    ));
    while order.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    // Queued behind the held boot: a subagent, an abandoned subagent, then the foreground.
    let subagent = tokio::spawn(admitted_boot(
        Arc::clone(&source),
        Priority::Subagent,
        boot("subagent", None),
    ));
    let abandoned = tokio::spawn(admitted_boot(
        Arc::clone(&source),
        Priority::Subagent,
        boot("abandoned", None),
    ));
    tokio::task::yield_now().await;
    let foreground = tokio::spawn(admitted_boot(
        Arc::clone(&source),
        Priority::Foreground,
        boot("foreground", None),
    ));
    tokio::task::yield_now().await;
    abandoned.abort();
    let _ = abandoned.await;
    release_first.send(()).unwrap();
    let first = first.await.unwrap();
    let foreground = foreground.await.unwrap();
    let subagent = subagent.await.unwrap();
    assert_eq!(
        (
            first.queued_ms,
            first.result.unwrap(),
            foreground.result.unwrap(),
            subagent.result.unwrap(),
        ),
        (0, "first", "foreground", "subagent")
    );
    assert_eq!(*order.lock().unwrap(), ["first", "foreground", "subagent"]);
}

/// A remote-style source: nothing is free now, and each acquire resolves only when the test
/// lets it, counting the permits returned.
struct LateSource {
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    returned: Arc<AtomicUsize>,
}

impl AdmissionSource for LateSource {
    fn acquire(&self, _request: AdmissionRequest) -> GrantFuture {
        let release = self.release.lock().unwrap().take();
        let returned = Arc::clone(&self.returned);
        Box::pin(async move {
            if let Some(release) = release {
                let _ = release.await;
            }
            Grant::new(
                0,
                Box::new(move |_observation| {
                    returned.fetch_add(1, Ordering::SeqCst);
                    false
                }),
            )
        })
    }
}

#[tokio::test]
async fn a_permit_granted_after_its_boot_gave_up_goes_straight_back() {
    let (grant_late, release) = tokio::sync::oneshot::channel();
    let returned = Arc::new(AtomicUsize::new(0));
    let source: Arc<dyn AdmissionSource> = Arc::new(LateSource {
        release: Mutex::new(Some(release)),
        returned: Arc::clone(&returned),
    });
    let booted = Arc::new(AtomicUsize::new(0));
    let ran = Arc::clone(&booted);
    let waiting = tokio::spawn(admitted_boot(source, Priority::Subagent, async move {
        ran.fetch_add(1, Ordering::SeqCst);
        anyhow::Ok(())
    }));
    tokio::task::yield_now().await;
    waiting.abort();
    let _ = waiting.await;
    // The source grants only now, after the waiter is gone.
    grant_late.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while returned.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        (
            booted.load(Ordering::SeqCst),
            returned.load(Ordering::SeqCst)
        ),
        (0, 1)
    );
}
