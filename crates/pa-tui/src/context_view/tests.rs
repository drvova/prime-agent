//! The context window: whole-frame renders at 80, 60, and 40 columns over a landed breakdown,
//! the frame it paints before the reply lands, and the keys.

use pa_types::context_breakdown::{
    AgentContextRow, CacheTotals, CategoryUsage, CompactionHistory, LastCompaction, RequestCache,
    SessionCost, ThinningSavings,
};

use super::*;
use crate::chrome::ContextUsage;
use crate::theme::ColorMode;

const NOW: i64 = 1_795_003_200_000;

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

/// The tray's state: 23k of a 128k window, autocompact at 80%, a reasoning model, $0.42 spent.
fn chrome() -> ChromeState {
    ChromeState {
        context: Some(ContextUsage {
            tokens: 23_000,
            context_window: 128_000,
            reading: crate::chrome::ContextReading::Measured,
            last_request: None,
        }),
        compaction_percent: Some(80.0),
        model_id: Some("faux-1".to_string()),
        thinking_suffix: Some("high".to_string()),
        cost_usd: Some(0.42),
        ..ChromeState::default()
    }
}

fn usage(tokens: u64, items: u64) -> CategoryUsage {
    CategoryUsage { tokens, items }
}

fn breakdown() -> ContextBreakdown {
    ContextBreakdown {
        categories: Some(ContextCategories {
            system_static: usage(6_200, 1),
            system_dynamic: usage(1_100, 1),
            tools: usage(2_300, 3),
            messages: usage(4_000, 12),
            tool_calls: usage(900, 6),
            tool_results: usage(7_400, 6),
            images: usage(1_200, 1),
            thinking: usage(0, 0),
            harness_digest: usage(300, 1),
            thinning: Some(ThinningSavings {
                thinned_results: 2,
                tokens_saved: 3_400,
            }),
        }),
        categories_error: None,
        cache: CacheTotals {
            input: 4_000,
            output: 2_000,
            cache_read: 90_000,
            cache_write: 6_000,
            last_request: Some(RequestCache {
                input: 500,
                cache_read: 19_000,
                cache_write: 500,
            }),
        },
        compaction: CompactionHistory {
            count: 1,
            last: Some(LastCompaction {
                tokens_before: 101_000,
                at_ms: Some((NOW - 12 * 60_000) as u64),
            }),
        },
        cost: SessionCost {
            own: 0.30,
            total: 0.42,
        },
        agents: vec![
            AgentContextRow {
                label: "researcher".to_string(),
                depth: 1,
                status: "running".to_string(),
                context_tokens: Some(9_000),
                context_window: Some(128_000),
                cost: 0.10,
            },
            AgentContextRow {
                label: "helper".to_string(),
                depth: 2,
                status: "idle".to_string(),
                context_tokens: None,
                context_window: Some(128_000),
                cost: 0.02,
            },
        ],
    }
}

fn text(lines: &[Line]) -> Vec<String> {
    lines
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

fn render(window: &mut ContextView, width: usize) -> Vec<String> {
    let kb = KeybindingsManager::new();
    let lines = window.render(&theme(), width, &kb, &chrome(), 60, NOW);
    for line in &lines {
        assert!(
            crate::width::line_width(line) <= width,
            "a row overflows {width} columns: {:?}",
            text(std::slice::from_ref(line))
        );
    }
    text(&lines)
}

fn landed() -> ContextView {
    let mut window = ContextView::new(ContextViewSource::Click, None);
    window.apply_breakdown(Ok(breakdown()));
    window
}

/// The full window: the tray's numbers, the bar with its autocompact tick, every category the
/// reply measured (the empty thinking row stays out), the cache, compaction, cost, subagent, and
/// model sections, and the hints.
#[test]
fn the_filled_window_at_80_columns() {
    let rows = render(&mut landed(), 80);
    assert_eq!(rows[0], "\u{2500}".repeat(80));
    assert_eq!(
        rows[1..],
        [
            "  Context",
            "",
            "    23k / 128k tokens \u{b7} 18% used",
            "    \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2503}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "    Autocompact at 80% (102k) \u{b7} 79k to go",
            "",
            "  Breakdown (estimated)",
            "    System prompt   6.2k   4.8%",
            "    Dynamic tail    1.1k   0.9%",
            "    Tool schemas    2.3k   1.8%  3 tools",
            "    Messages        4.0k   3.1%  12 messages",
            "    Tool calls       900   0.7%  6 calls",
            "    Tool results    7.4k   5.8%  6 results",
            "    Images          1.2k   0.9%  1 image",
            "    Harness digest   300   0.2%",
            "    Thinned away    3.4k         2 results thinned",
            "    Total            23k  18.3%",
            "",
            "  Prompt cache",
            "    read 90k \u{b7} write 6.0k \u{b7} fresh 4.0k \u{b7} hit 90% (last 95%)",
            "",
            "  Compaction",
            "    1 so far \u{b7} the last replaced 101k, 12m ago",
            "",
            "  Cost",
            "    $0.42 session \u{b7} main $0.30 \u{b7} subagents $0.12",
            "",
            "  Subagents",
            "    researcher  9.0k/128k 7%  $0.10  running",
            "      helper               \u{2014}  $0.02  idle",
            "",
            "  Model",
            "    faux-1 \u{b7} thinking high \u{b7} 128k window",
            "",
            "    /compact compacts now \u{b7} /context all lists every agent",
            "  Esc close",
            "",
        ]
    );
}

/// At 60 columns every row still fits whole: the same frame under a narrower rule.
#[test]
fn the_window_at_60_columns_loses_nothing() {
    let wide = render(&mut landed(), 80);
    let rows = render(&mut landed(), 60);
    assert_eq!(rows[0], "\u{2500}".repeat(60));
    assert_eq!(rows[1..], wide[1..]);
}

/// At 40 columns the bar shortens, the stat rows drop their details (then their share column)
/// before a label would truncate, and the prose rows wrap at their separators.
#[test]
fn the_window_at_40_columns_degrades_row_by_row() {
    let rows = render(&mut landed(), 40);
    assert_eq!(rows[0], "\u{2500}".repeat(40));
    assert_eq!(
        rows[1..],
        [
            "  Context",
            "",
            "    23k / 128k tokens \u{b7} 18% used",
            "    \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2503}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "    Autocompact at 80% (102k)",
            "    79k to go",
            "",
            "  Breakdown (estimated)",
            "    System prompt   6.2k   4.8%",
            "    Dynamic tail    1.1k   0.9%",
            "    Tool schemas    2.3k   1.8%  3 tools",
            "    Messages        4.0k   3.1%",
            "    Tool calls       900   0.7%  6 calls",
            "    Tool results    7.4k   5.8%",
            "    Images          1.2k   0.9%  1 image",
            "    Harness digest   300   0.2%",
            "    Thinned away    3.4k",
            "    Total            23k  18.3%",
            "",
            "  Prompt cache",
            "    read 90k \u{b7} write 6.0k \u{b7} fresh 4.0k",
            "    hit 90% (last 95%)",
            "",
            "  Compaction",
            "    1 so far",
            "    the last replaced 101k, 12m ago",
            "",
            "  Cost",
            "    $0.42 session \u{b7} main $0.30",
            "    subagents $0.12",
            "",
            "  Subagents",
            "    researcher  9.0k/128k 7%  $0.10",
            "      helper               \u{2014}  $0.02",
            "",
            "  Model",
            "    faux-1 \u{b7} thinking high \u{b7} 128k window",
            "",
            "    /compact compacts now",
            "    /context all lists every agent",
            "  Esc close",
            "",
        ]
    );
}

/// Before the reply lands the window already paints the tray's numbers and the model; the
/// breakdown reads as measuring and the cost as the title bar's spend.
#[test]
fn the_first_frame_paints_before_the_reply() {
    let mut window = ContextView::new(ContextViewSource::Command, None);
    let rows = render(&mut window, 80);
    assert_eq!(
        rows[3..],
        [
            "    23k / 128k tokens · 18% used",
            rows[4].as_str(),
            "    Autocompact at 80% (102k) · 79k to go",
            "",
            "  Breakdown (estimated)",
            "    Measuring…",
            "",
            "  Cost",
            "    $0.42 session",
            "",
            "  Model",
            "    faux-1 · thinking high · 128k window",
            "",
            "    /compact compacts now · /context all lists every agent",
            "  Esc close",
            "",
        ]
    );
    assert_eq!(
        window.summary(),
        ContextViewSummary {
            source: ContextViewSource::Command,
            breakdown: ContextBreakdownOutcome::Loading,
            category_count: 0,
            subagent_count: 0,
        }
    );
}

#[test]
fn escape_closes_and_the_summary_counts_the_rows() {
    let kb = KeybindingsManager::new();
    let mut window = landed();
    assert_eq!(window.handle_key("down", &kb), ContextViewAction::None);
    assert_eq!(window.handle_key("escape", &kb), ContextViewAction::Close);
    assert_eq!(
        window.summary(),
        ContextViewSummary {
            source: ContextViewSource::Click,
            breakdown: ContextBreakdownOutcome::Loaded,
            category_count: 8,
            subagent_count: 2,
        }
    );
}
