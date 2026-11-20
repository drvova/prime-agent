use super::*;

fn answered() -> ProviderResult<SearchResponse> {
    Ok(SearchResponse::from_sources(Vec::new()))
}

fn failed(status: Option<u16>) -> ProviderResult<SearchResponse> {
    Err(ProviderError::new("x", status))
}

#[test]
fn blocks_cool_an_engine_down_longer_each_time_and_an_answer_resets_it() {
    let steps = [
        (answered(), 400),
        (answered(), 800),
        (failed(Some(429)), 50),
        (failed(Some(429)), 50),
        (failed(Some(503)), 50),
        (answered(), 600),
    ];
    let mut health = Health::default();
    let observed: Vec<(Outcome, Option<u64>, Option<u64>, Health)> = steps
        .iter()
        .map(|(result, latency_ms)| {
            let (observation, next) = observe(result, *latency_ms, health);
            health = next;
            (
                observation.outcome,
                observation.latency_ms,
                observation.limits.retry_after_ms,
                health,
            )
        })
        .collect();
    assert_eq!(
        observed,
        [
            (
                Outcome::Success,
                Some(400),
                None,
                Health {
                    latency_ms: Some(400),
                    blocks: 0
                }
            ),
            (
                Outcome::Success,
                Some(800),
                None,
                Health {
                    latency_ms: Some(500),
                    blocks: 0
                }
            ),
            (
                Outcome::Throttled,
                None,
                Some(60_000),
                Health {
                    latency_ms: Some(500),
                    blocks: 1
                }
            ),
            (
                Outcome::Throttled,
                None,
                Some(120_000),
                Health {
                    latency_ms: Some(500),
                    blocks: 2
                }
            ),
            (
                Outcome::Failed,
                None,
                None,
                Health {
                    latency_ms: Some(500),
                    blocks: 2
                }
            ),
            (
                Outcome::Success,
                Some(600),
                None,
                Health {
                    latency_ms: Some(525),
                    blocks: 0
                }
            ),
        ]
    );
}

#[tokio::test]
async fn a_blocking_engine_is_skipped_until_its_cooldown_passes() {
    let limits = EngineLimits::new();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let call = |result: ProviderResult<SearchResponse>| {
        let calls = &calls;
        async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            result
        }
    };
    let first = limits
        .run(ProviderId::Startpage, call(failed(Some(429))))
        .await;
    let second = limits.run(ProviderId::Startpage, call(answered())).await;
    // Another engine is unaffected.
    let other = limits.run(ProviderId::Mojeek, call(answered())).await;
    assert_eq!(
        (
            first.map(|_| ()).map_err(|error| error.status),
            second.map(|_| ()).map_err(|error| error.status),
            other.map(|_| ()).map_err(|error| error.status),
            calls.load(std::sync::atomic::Ordering::SeqCst),
        ),
        (Err(Some(429)), Err(Some(429)), Ok(()), 2)
    );
}

#[test]
fn the_soft_deadline_follows_each_engine_latency() {
    let limits = EngineLimits::new();
    limits.note_answer(ProviderId::Duckduckgo, 300);
    limits.note_answer(ProviderId::Ecosia, 2_000);
    limits.note_answer(ProviderId::Mojeek, 4_000);
    assert_eq!(
        [
            ProviderId::Duckduckgo,
            ProviderId::Ecosia,
            ProviderId::Mojeek,
            ProviderId::Startpage,
        ]
        .map(|engine| limits.soft_deadline(engine)),
        [
            Duration::from_secs(1),
            Duration::from_secs(4),
            Duration::from_secs(5),
            Duration::from_secs(5),
        ]
    );
}
