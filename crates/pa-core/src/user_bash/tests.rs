use super::output::{sanitize_output, truncate_tail};
use super::*;

#[test]
fn an_abort_stays_with_every_admitted_run_when_another_run_starts() {
    let user_bash = UserBash::new();
    let streamed = user_bash.claim().expect("claim slot");
    let first = user_bash.begin_awaited();
    let second = user_bash.begin_awaited();
    user_bash.abort();
    let later = user_bash.begin_awaited();
    assert_eq!(
        [
            streamed.is_cancelled(),
            first.cancellation().is_cancelled(),
            second.cancellation().is_cancelled(),
            later.cancellation().is_cancelled()
        ],
        [true, true, true, false],
    );
    user_bash.abort();
    assert!(later.cancellation().is_cancelled());
    user_bash.release();
    drop((first, second, later));
    assert!(!user_bash.is_running());
}

#[test]
fn an_abandoned_awaited_run_releases_the_flag() {
    let user_bash = UserBash::new();
    assert!(!user_bash.is_running());
    {
        let _awaited = user_bash.begin_awaited();
        assert!(user_bash.is_running());
    }
    assert!(
        !user_bash.is_running(),
        "the dropped bracket left the awaited count stuck on"
    );
}

#[test]
fn sanitize_output_matches_the_ts_chain() {
    assert_eq!(sanitize_output("clean\r\noutput"), "clean\noutput");
    assert_eq!(
        sanitize_output("\u{1b}[32mgreen\u{1b}[0m plain"),
        "green plain"
    );
    assert_eq!(
        sanitize_output("keep\ttab\nand\x00drop"),
        "keep\ttab\nanddrop"
    );
    assert_eq!(sanitize_output("format\u{fff9}gone"), "formatgone");
}

#[test]
fn truncate_tail_keeps_the_tail_budget() {
    let (content, truncated) = truncate_tail("one\ntwo\nthree\n");
    assert!(!truncated);
    assert_eq!(content, "one\ntwo\nthree\n");

    // A long output keeps only its tail (the TS window).
    let big = "x".repeat(DEFAULT_MAX_BYTES + 10_000);
    let (kept, truncated) = truncate_tail(&big);
    assert!(truncated);
    assert!(kept.len() <= DEFAULT_MAX_BYTES);
    assert!(big.ends_with(&kept), "the tail wins");
}
