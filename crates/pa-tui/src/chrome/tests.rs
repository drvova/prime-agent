/// Separators stay inert, and a truncated row keeps no region for dropped groups.
#[test]
fn the_dock_segments_cover_each_groups_own_cells() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        subagents_running_direct: 2,
        heartbeats: 3,
        heartbeats_paused: 1,
        shells: items(1),
        factory_group: true,
        ..ActivityDock::default()
    };
    let (frame, segments) = render_activity_dock_segments(&dock, &theme, 120);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        " ◆ 2 subagents  ·  ◷ 3 heartbeats · ◐ 1 paused  ·  ▸ 1 shell  ·  ⚙ 0 factory"
    );
    let ordered: Vec<ActivityGroup> = segments.iter().map(|segment| segment.group).collect();
    assert_eq!(
        ordered,
        vec![
            ActivityGroup::Subagents,
            ActivityGroup::Heartbeats,
            ActivityGroup::Bash,
            ActivityGroup::Factory,
        ],
        "the segments follow the row's group order"
    );
    // The heartbeat segment covers the paused cluster too (the cluster
    // is the heartbeats group's own readout).
    let of = |text: &str| text.find(text).map(|_| 0);
    let _ = of;
    let subagents = &segments[0].cols;
    let heartbeats = &segments[1].cols;
    let bash = &segments[2].cols;
    // The recorded columns are display cells, and the row's glyphs are
    // one cell wide: slice the text by char, never by byte.
    let cells = |cols: &std::ops::Range<usize>| -> String {
        text.chars()
            .skip(cols.start)
            .take(cols.end - cols.start)
            .collect()
    };
    assert_eq!(cells(subagents), "◆ 2 subagents");
    assert_eq!(cells(heartbeats), "◷ 3 heartbeats · ◐ 1 paused");
    assert_eq!(cells(bash), "▸ 1 shell");
    let between = |a: &std::ops::Range<usize>, b: &std::ops::Range<usize>| -> String {
        text.chars().skip(a.end).take(b.start - a.end).collect()
    };
    assert_eq!(between(subagents, heartbeats), "  ·  ");
    assert_eq!(between(heartbeats, bash), "  ·  ");
    // A truncation that drops the trailing group keeps no region for
    // it: the segments clamp to the row the terminal kept.
    let (frame, segments) = render_activity_dock_segments(&dock, &theme, 20);
    let used = crate::width::spans_width(&frame[1]);
    assert!(used <= 20, "the row truncates inside the width");
    for segment in &segments {
        assert!(
            segment.cols.start < used,
            "a dropped segment records no span"
        );
        assert!(segment.cols.end <= used);
    }
    let dock = ActivityDock {
        goal_label: Some("Pursuing goal (0s)".to_string()),
        ..ActivityDock::default()
    };
    let (_, segments) = render_activity_dock_segments(&dock, &theme, 120);
    assert_eq!(
        segments.last().map(|segment| segment.group),
        Some(ActivityGroup::Goal)
    );
}

/// The hint spans exactly the arrow and the label, never the depth metadata beside them.
#[test]
fn the_tray_reports_the_manage_hints_own_cells() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let state = ChromeState {
        show_manage: true,
        tray_depth: Some(2),
        ..Default::default()
    };
    let (line, hint) = render_tray_with_hint(&state, &theme, 120);
    let hint = hint.expect("the manage hint reports its cells");
    let text = line
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text.chars()
            .skip(hint.start)
            .take(hint.end - hint.start)
            .collect::<String>(),
        "\u{2190} manage"
    );
    assert_eq!(
        text.chars().skip(hint.end).take(2).collect::<String>(),
        "  ",
        "the depth label stays outside the hint's span"
    );
    let state = ChromeState {
        show_manage: true,
        tray_override: Some("Press ctrl+c again to exit".to_string()),
        ..Default::default()
    };
    let (_, hint) = render_tray_with_hint(&state, &theme, 120);
    assert_eq!(hint, None, "an override label carries no hint region");
    let state = ChromeState::default();
    let (_, hint) = render_tray_with_hint(&state, &theme, 120);
    assert_eq!(hint, None, "a hidden manage hint carries no region");
}

#[test]
fn activity_dock_frames_one_row_with_running_paused_and_goal_counts() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        subagents_running_direct: 1,
        subagents_running_nested: 1,
        heartbeats: 3,
        heartbeats_paused: 1,
        shells: items(1),
        factory_group: true,
        goal_label: Some("Pursuing goal (0s)".to_string()),
        ..ActivityDock::default()
    };
    // The heartbeat cluster and the goal label widen the row: the
    // fixture renders at 120 so the full line stays untruncated.
    let frame = render_activity_dock(&dock, &theme, 120);
    assert_eq!(frame.len(), 2, "a muted separator rule plus the row");
    let rule = frame[0]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(rule.chars().next(), Some('─'));
    assert_eq!(rule.chars().count(), 120);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        " ◆ 2 subagents  ·  ◷ 3 heartbeats · ◐ 1 paused  ·  ▸ 1 shell  ·  ⚙ 0 factory  ·  Pursuing goal (0s)"
    );
    // The operator's 2026-09-24 color-coding directive.
    let success = theme.fg_style(ThemeColor::Success).fg;
    let colored = |text: &str, color| {
        frame[1]
            .iter()
            .any(|span| span.content.contains(text) && span.style.fg == color)
    };
    assert!(colored("◆ 2 subagents", success));
    assert!(colored("◷ 3 heartbeats", success));
    assert!(colored("▸ 1 shell", success));
    assert!(colored("Pursuing goal", success));
    // A paused goal stays on the dock (the tray cluster is gone) in
    // the warning color — every live goal state keeps a surface.
    let dock = ActivityDock {
        goal_label: Some("Goal paused (0s)".to_string()),
        factory_group: true,
        ..ActivityDock::default()
    };
    let frame = render_activity_dock(&dock, &theme, 100);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    let warning = theme.fg_style(ThemeColor::Warning).fg;
    assert!(
        text.contains("Goal paused (0s)"),
        "the paused row renders: {text}"
    );
    assert!(
        frame[1]
            .iter()
            .any(|span| span.content.contains("Goal paused") && span.style.fg == warning),
        "the paused goal reads amber"
    );
    // A running count of zero still renders: a long idle roster reads
    // as quiet, not as uniformly busy.
    let dock = ActivityDock {
        heartbeats: 1,
        factory_group: true,
        ..ActivityDock::default()
    };
    let frame = render_activity_dock(&dock, &theme, 100);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        " ◆ 0 subagents  ·  ◷ 1 heartbeat  ·  ▸ 0 shells  ·  ⚙ 0 factory"
    );
    // The all-zero dock renders its own empty state — the zero
    // readout — and the zero segments stay neutral, never green. The
    // factory lane is advertised here (the unadvertised lane's row is
    // the pin of its own).
    let advertised = ActivityDock {
        factory_group: true,
        ..ActivityDock::default()
    };
    let frame = render_activity_dock(&advertised, &theme, 100);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        " \u{25c6} 0 subagents  \u{b7}  \u{25f7} 0 heartbeats  \u{b7}  \u{25b8} 0 shells  \u{b7}  \u{2699} 0 factory"
    );
    assert!(frame[1]
        .iter()
        .all(|span| span.style.fg != theme.fg_style(ThemeColor::Success).fg));
    // The row shrinks in its stated order, whole steps only: tight separators, the lane
    // words, the zero-count lanes, the paused cluster, then the goal — never a cut word.
    let dock = ActivityDock {
        subagents_running_direct: 1,
        subagents_running_nested: 2,
        heartbeats: 4,
        heartbeats_paused: 2,
        shells: items(2),
        goal_label: Some("Pursuing goal (12m 05s)".to_string()),
        ..ActivityDock::default()
    };
    let rows: Vec<String> = [100, 80, 60, 45, 30, 20]
        .into_iter()
        .map(|width| {
            render_activity_dock(&dock, &theme, width)[1]
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect();
    let counts = " \u{25c6} 3 \u{b7} \u{25f7} 4 \u{b7} \u{25d0} 2 paused \u{b7} \u{25b8} 2 \u{b7} Pursuing goal (12m 05s)";
    assert_eq!(
        rows,
        vec![
            " \u{25c6} 3 subagents  \u{b7}  \u{25f7} 4 heartbeats \u{b7} \u{25d0} 2 paused  \u{b7}  \u{25b8} 2 shells  \u{b7}  Pursuing goal (12m 05s)".to_string(),
            counts.to_string(),
            counts.to_string(),
            " \u{25c6} 3 \u{b7} \u{25f7} 4 \u{b7} \u{25b8} 2 \u{b7} Pursuing goal (12m 05s)".to_string(),
            " \u{25c6} 3 \u{b7} \u{25f7} 4 \u{b7} \u{25b8} 2".to_string(),
            " \u{25c6} 3 \u{b7} \u{25f7} 4 \u{b7} \u{25b8} 2".to_string(),
        ]
    );
    // The all-zero dock keeps its words while they fit, then its counts, and only a row
    // narrower than the counts drops the idle lanes.
    let idle = ActivityDock::default();
    let rows: Vec<String> = [48, 30, 12]
        .into_iter()
        .map(|width| {
            render_activity_dock(&idle, &theme, width)[1]
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            " \u{25c6} 0 subagents  \u{b7}  \u{25f7} 0 heartbeats  \u{b7}  \u{25b8} 0 shells"
                .to_string(),
            " \u{25c6} 0 \u{b7} \u{25f7} 0 \u{b7} \u{25b8} 0".to_string(),
            " \u{25c6} 0".to_string(),
        ]
    );
    // A row that fits whole keeps every character — the ellipsis
    // column is only borrowed when truncation actually happens.
    let frame = render_activity_dock(&dock, &theme, 120);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert!(
        !text.contains('\u{2026}'),
        "the untruncated row keeps its characters: {text:?}"
    );
    assert!(text.contains("Pursuing goal (12m 05s)"));
}

/// A paused factory run never reads as running: the collapsed count holds only the running
/// run (green), and the paused one sits in its own amber cluster, like a paused heartbeat.
#[test]
fn the_collapsed_dock_counts_paused_factory_runs_apart_from_running_ones() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let colored = |dock: &ActivityDock| {
        render_activity_dock(dock, &theme, 120)[1]
            .iter()
            .skip_while(|span| !span.content.starts_with('⚙'))
            .map(|span| (span.content.clone(), span.style.fg))
            .collect::<Vec<_>>()
    };
    let paused_only = ActivityDock {
        factory_runs: vec!["nightly paused".to_string()],
        factory_counts: crate::factory_view::FactoryRunCounts {
            running: 0,
            paused: 1,
        },
        factory_group: true,
        ..ActivityDock::default()
    };
    let muted = theme.fg_style(ThemeColor::Muted).fg;
    let success = theme.fg_style(ThemeColor::Success).fg;
    let warning = theme.fg_style(ThemeColor::Warning).fg;
    let dim = theme.fg_style(ThemeColor::Dim).fg;
    assert_eq!(
        colored(&paused_only),
        vec![
            ("⚙ 0 factory".to_string(), muted),
            (" · ".to_string(), dim),
            ("◐ 1 paused".to_string(), warning),
        ]
    );
    let mixed = ActivityDock {
        factory_runs: vec!["nightly paused".to_string(), "lint running".to_string()],
        factory_counts: crate::factory_view::FactoryRunCounts {
            running: 1,
            paused: 1,
        },
        ..paused_only
    };
    assert_eq!(
        colored(&mixed),
        vec![
            ("⚙ 1 factory".to_string(), success),
            (" · ".to_string(), dim),
            ("◐ 1 paused".to_string(), warning),
        ]
    );
}

/// ONE number: direct children and nested descendants summed, never a `direct, nested` pair.
#[test]
fn prompt_bar_subagent_segment_is_the_running_count_only() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    // Two directly-running children, nothing nested.
    let dock = ActivityDock {
        subagents_running_direct: 2,
        subagents_running_nested: 0,
        factory_group: true,
        ..ActivityDock::default()
    };
    let frame = render_activity_dock(&dock, &theme, 80);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        " ◆ 2 subagents  ·  ◷ 0 heartbeats  ·  ▸ 0 shells  ·  ⚙ 0 factory"
    );
    // Two running children plus seven running descendants: ONE
    // number — the summed total, never the pair, never the
    // descendant total, and no category breakdown.
    let dock = ActivityDock {
        subagents_running_direct: 2,
        subagents_running_nested: 7,
        factory_group: true,
        ..ActivityDock::default()
    };
    let frame = render_activity_dock(&dock, &theme, 80);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        " ◆ 9 subagents  ·  ◷ 0 heartbeats  ·  ▸ 0 shells  ·  ⚙ 0 factory"
    );
    assert!(!text.contains("2,"), "no direct/nested pair: {text}");
    assert!(!text.contains("idle"), "no category breakdown: {text}");
    assert!(
        !text.contains('5'),
        "the descendant total never renders: {text}"
    );
    let dock = ActivityDock {
        subagents_running_direct: 0,
        subagents_running_nested: 1,
        factory_group: true,
        ..ActivityDock::default()
    };
    let frame = render_activity_dock(&dock, &theme, 80);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text,
        " ◆ 1 subagent  ·  ◷ 0 heartbeats  ·  ▸ 0 shells  ·  ⚙ 0 factory"
    );
}

/// The plain text of each rendered line.
fn line_texts(frame: &[Line]) -> Vec<String> {
    frame
        .iter()
        .map(|line| line.iter().map(|span| span.content.as_str()).collect())
        .collect()
}

/// Focus expands the dock into one row per group in `groups()` order, each with its details;
/// the unfocused dock is the single collapsed line.
#[test]
fn the_focused_dock_expands_into_one_row_per_group() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        subagents_running_direct: 2,
        subagents_running_nested: 1,
        subagents_idle: 1,
        subagents_inactive: 2,
        heartbeats: 2,
        heartbeats_active: vec!["5m lane canary".to_string()],
        heartbeats_paused: 1,
        shells: vec!["cargo test · 12.0s".to_string()],
        shells_persistent: 0,
        factory_specs: 2,
        factory_runs: vec!["nightly paused".to_string()],
        factory_counts: crate::factory_view::FactoryRunCounts {
            running: 0,
            paused: 1,
        },
        factory_group: true,
        goal_label: Some("Pursuing goal (0s)".to_string()),
        selected: ActivityGroup::Subagents,
        focused: true,
    };
    let (frame, segments) = render_activity_dock_segments(&dock, &theme, 80);
    let texts = line_texts(&frame);
    assert_eq!(texts[0], "─".repeat(80), "the rule stays on top");
    let rows: Vec<&str> = texts[1..].iter().map(|row| row.trim_end()).collect();
    assert_eq!(
        rows,
        vec![
            " ◆ subagents   3 running · 1 idle · 2 inactive",
            " ◷ heartbeats  5m lane canary",
            " ▸ shells      1 running · 0 persistent  ·  cargo test · 12.0s",
            " ⚙ factory     2 factories · 0 running · 1 paused  ·  nightly paused",
            " Pursuing goal (0s)",
        ]
    );
    assert_eq!(
        segments
            .iter()
            .map(|segment| (segment.group, segment.row))
            .collect::<Vec<_>>(),
        dock.groups().into_iter().zip(1..).collect::<Vec<_>>(),
        "every row is its group's click surface"
    );
    let collapsed = ActivityDock {
        focused: false,
        ..dock
    };
    let texts = line_texts(&render_activity_dock(&collapsed, &theme, 120));
    assert_eq!(
        texts[1..],
        [" ◆ 3 subagents  ·  ◷ 2 heartbeats · ◐ 1 paused  ·  ▸ 1 shell  ·  ⚙ 0 factory · ◐ 1 paused  ·  Pursuing goal (0s)"]
    );
}

/// The selected row carries the ONE shared selection band across the full width (the hover's
/// own color, never the accent) while its spans keep their own colors; no other row is banded,
/// and the collapsed line never is.
#[test]
fn the_expanded_dock_bands_exactly_the_selected_row() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        heartbeats_active: items(2),
        shells: items(1),
        selected: ActivityGroup::Heartbeats,
        focused: true,
        ..ActivityDock::default()
    };
    let frame = render_activity_dock(&dock, &theme, 60);
    let band = theme.selection_row_style();
    assert_eq!(band.bg, theme.hover_row_style().bg);
    assert_ne!(band.bg, theme.fg_style(ThemeColor::Accent).fg);
    let heartbeats = &frame[2];
    assert_eq!(
        crate::width::spans_width(heartbeats),
        60,
        "the band spans the row"
    );
    assert!(heartbeats.iter().all(|span| span.style.bg == band.bg));
    let title = heartbeats
        .iter()
        .find(|span| span.content == "◷ heartbeats")
        .expect("the heartbeats title renders");
    assert_eq!(title.style.fg, theme.fg_style(ThemeColor::Success).fg);
    for row in [&frame[1], &frame[3]] {
        assert!(row.iter().all(|span| span.style.bg != band.bg));
    }
    let collapsed = ActivityDock {
        focused: false,
        ..dock
    };
    let frame = render_activity_dock(&collapsed, &theme, 60);
    assert!(frame[1].iter().all(|span| span.style.bg.is_none()));
}

/// A row lists as many items as fit and counts the rest as `+N`; the shells row leads with its
/// running and persistent counts, and the marker counts the commands it hides.
#[test]
fn an_overflowing_expanded_row_counts_the_rest() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        shells: items(9),
        shells_persistent: 2,
        selected: ActivityGroup::Subagents,
        focused: true,
        ..ActivityDock::default()
    };
    let texts = line_texts(&render_activity_dock(&dock, &theme, 60));
    assert_eq!(
        texts[3],
        " ▸ shells      9 running · 2 persistent  ·  item 1  +8"
    );
}

/// Stored factory specs without a live run still fill the expanded factory row with its counts,
/// while the collapsed segment keeps counting live runs only.
#[test]
fn the_expanded_factory_row_counts_stored_specs_without_runs() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        factory_specs: 2,
        factory_group: true,
        selected: ActivityGroup::Subagents,
        focused: true,
        ..ActivityDock::default()
    };
    let texts = line_texts(&render_activity_dock(&dock, &theme, 80));
    assert_eq!(
        texts[4].trim_end(),
        " ⚙ factory     2 factories · 0 running"
    );
    let collapsed = ActivityDock {
        focused: false,
        ..dock
    };
    let texts = line_texts(&render_activity_dock(&collapsed, &theme, 80));
    assert!(texts[1].ends_with("⚙ 0 factory"), "{texts:?}");
}

/// A large activity list fits by terminal cells, including a multi-digit omitted count.
#[test]
fn a_large_expanded_row_fits_wide_items_and_counts_the_omitted_items() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        heartbeats_active: vec!["界界".to_string(); 4096],
        focused: true,
        ..ActivityDock::default()
    };
    let texts = line_texts(&render_activity_dock(&dock, &theme, 60));
    assert_eq!(
        texts[2],
        " ◷ heartbeats  界界  ·  界界  ·  界界  ·  界界  +4092"
    );
    assert_eq!(fitting_items(&dock.heartbeats_active, 4096 * 9 - 5), 4096);
}

/// A first item too long to sit beside the `+N` marker is ellipsized, so the omitted count still
/// renders.
#[test]
fn an_oversized_first_item_keeps_the_overflow_marker() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        heartbeats_active: vec![
            format!("5m {}", "check the deploy queue ".repeat(4)),
            "9m lane canary".to_string(),
        ],
        selected: ActivityGroup::Subagents,
        focused: true,
        ..ActivityDock::default()
    };
    let texts = line_texts(&render_activity_dock(&dock, &theme, 40));
    assert_eq!(texts[2], " ◷ heartbeats  5m check the deploy …  +1");
}

/// One press steps to the neighboring rendered group and wraps; N groups take exactly N presses.
#[test]
fn dock_arrows_visit_every_group_even_when_empty() {
    // The all-zero dock with the factory lane advertised: the
    // heartbeats, shells, and factory groups are empty and stay in
    // the cycle.
    let dock = ActivityDock {
        factory_group: true,
        ..ActivityDock::default()
    };
    assert_eq!(
        dock.groups(),
        vec![
            ActivityGroup::Subagents,
            ActivityGroup::Heartbeats,
            ActivityGroup::Bash,
            ActivityGroup::Factory,
        ]
    );
    assert_eq!(
        dock.step(ActivityGroup::Subagents, ActivityDirection::Next),
        ActivityGroup::Heartbeats
    );
    assert_eq!(
        dock.step(ActivityGroup::Heartbeats, ActivityDirection::Next),
        ActivityGroup::Bash
    );
    assert_eq!(
        dock.step(ActivityGroup::Bash, ActivityDirection::Next),
        ActivityGroup::Factory
    );
    assert_eq!(
        dock.step(ActivityGroup::Factory, ActivityDirection::Next),
        ActivityGroup::Subagents,
        "the cycle wraps past the last group"
    );
    // Up: the same groups in reverse, wrapping past the first.
    assert_eq!(
        dock.step(ActivityGroup::Subagents, ActivityDirection::Prev),
        ActivityGroup::Factory,
        "the cycle wraps past the first group"
    );
    assert_eq!(
        dock.step(ActivityGroup::Factory, ActivityDirection::Prev),
        ActivityGroup::Bash
    );
    assert_eq!(
        dock.step(ActivityGroup::Bash, ActivityDirection::Prev),
        ActivityGroup::Heartbeats
    );
    assert_eq!(
        dock.step(ActivityGroup::Heartbeats, ActivityDirection::Prev),
        ActivityGroup::Subagents
    );
    // N groups take exactly N presses to return to the start — the
    // emptiness of a group never moves another.
    for direction in [ActivityDirection::Next, ActivityDirection::Prev] {
        let mut walked = ActivityGroup::Subagents;
        let rendered = dock.groups().len();
        for presses in 1..=rendered {
            walked = dock.step(walked, direction);
            assert_eq!(
                walked == ActivityGroup::Subagents,
                presses == rendered,
                "the cycle length is exactly the rendered group count"
            );
        }
    }
}

/// Filling groups changes only the rendered counts, never the order,
/// the neighbors, or the press count.
#[test]
fn dock_arrows_visit_the_same_groups_with_items() {
    let dock = ActivityDock {
        subagents_running_direct: 1,
        subagents_running_nested: 2,
        heartbeats: 2,
        heartbeats_paused: 1,
        shells: items(1),
        factory_group: true,
        goal_label: Some("Pursuing goal (0s)".to_string()),
        ..ActivityDock::default()
    };
    assert_eq!(
        dock.groups(),
        vec![
            ActivityGroup::Subagents,
            ActivityGroup::Heartbeats,
            ActivityGroup::Bash,
            ActivityGroup::Factory,
            ActivityGroup::Goal,
        ]
    );
    // The full cycle right: every group in order, the goal group
    // included, back to the start in five presses.
    let mut walked = ActivityGroup::Subagents;
    for expected in [
        ActivityGroup::Heartbeats,
        ActivityGroup::Bash,
        ActivityGroup::Factory,
        ActivityGroup::Goal,
        ActivityGroup::Subagents,
    ] {
        walked = dock.step(walked, ActivityDirection::Next);
        assert_eq!(walked, expected);
    }
    let mut walked = ActivityGroup::Subagents;
    for expected in [
        ActivityGroup::Goal,
        ActivityGroup::Factory,
        ActivityGroup::Bash,
        ActivityGroup::Heartbeats,
        ActivityGroup::Subagents,
    ] {
        walked = dock.step(walked, ActivityDirection::Prev);
        assert_eq!(walked, expected);
    }
}

/// A stale selection steps to a group the dock still renders — never to a hidden segment.
#[test]
fn dock_goal_group_unmounts_with_its_row() {
    let with_goal = ActivityDock {
        goal_label: Some("Goal paused (0s)".to_string()),
        factory_group: true,
        ..ActivityDock::default()
    };
    assert!(with_goal.groups().contains(&ActivityGroup::Goal));
    let ended = ActivityDock {
        factory_group: true,
        ..ActivityDock::default()
    };
    assert!(
        !ended.groups().contains(&ActivityGroup::Goal),
        "the goal group leaves the cycle when its row unmounts"
    );
    assert_eq!(
        ended.step(ActivityGroup::Goal, ActivityDirection::Prev),
        ActivityGroup::Factory,
        "a stale goal selection lands on the row's last group"
    );
    assert_eq!(
        ended.step(ActivityGroup::Goal, ActivityDirection::Next),
        ActivityGroup::Heartbeats,
        "a stale goal selection steps from the row's first group"
    );
}

/// The factory group is the lane's opt-in surface: an unadvertised
/// `factory_activity` lane mounts no factory group anywhere — no row
/// segment, no traversal, no click region (the factory's default-off
/// gate; the dock's factory group renders exactly while the daemon
/// advertises the lane).
#[test]
fn an_unadvertised_factory_lane_mounts_no_factory_group() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    // The default dock is the unadvertised lane (the opt-in default).
    let dock = ActivityDock::default();
    assert!(
        !dock.groups().contains(&ActivityGroup::Factory),
        "the unadvertised lane stays out of the traversal cycle"
    );
    let (frame, segments) = render_activity_dock_segments(&dock, &theme, 120);
    let text = frame[1]
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(
        text, " ◆ 0 subagents  ·  ◷ 0 heartbeats  ·  ▸ 0 shells",
        "no factory segment renders: {text}"
    );
    assert!(
        segments
            .iter()
            .all(|segment| segment.group != ActivityGroup::Factory),
        "no factory click region records"
    );
    // The arrows wrap the remaining groups exactly: a stale Factory
    // selection steps to the row's real ends, never a hidden group.
    assert_eq!(
        dock.step(ActivityGroup::Bash, ActivityDirection::Next),
        ActivityGroup::Subagents,
        "the cycle wraps past the shells group"
    );
    assert_eq!(
        dock.step(ActivityGroup::Subagents, ActivityDirection::Prev),
        ActivityGroup::Bash,
        "the reverse cycle wraps past the subagents group"
    );
}

/// An empty group keeps its expanded row: its glyph and name render dimmed with no details,
/// and the row stays selectable.
#[test]
fn an_empty_group_renders_a_dimmed_row_and_stays_selectable() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let dock = ActivityDock {
        heartbeats_paused: 2,
        selected: ActivityGroup::Heartbeats,
        focused: true,
        factory_group: true,
        ..ActivityDock::default()
    };
    let frame = render_activity_dock(&dock, &theme, 60);
    let dim = theme.fg_style(ThemeColor::Dim).fg;
    let band = theme.selection_row_style();
    for (row, title) in
        frame[1..]
            .iter()
            .zip(["◆ subagents", "◷ heartbeats", "▸ shells", "⚙ factory"])
    {
        let text: String = row.iter().map(|span| span.content.as_str()).collect();
        assert_eq!(text.trim_end(), format!(" {title}"), "no details: {text:?}");
        let span = row
            .iter()
            .find(|span| span.content == title)
            .unwrap_or_else(|| panic!("the {title} row renders: {text:?}"));
        assert_eq!(span.style.fg, dim, "the empty {title} row reads dimmed");
    }
    assert!(
        frame[2].iter().all(|span| span.style.bg == band.bg),
        "the empty heartbeats row (paused heartbeats only) carries the selection"
    );
}

#[test]
fn speed_footer_is_one_dim_row_truncated_to_width() {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let row = render_speed_footer("188 tok/s · avg 200", &theme, 100);
    let text = row
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(text, "188 tok/s · avg 200");
    assert_eq!(row.len(), 1);
    let narrow = render_speed_footer("188 tok/s · avg 200", &theme, 10);
    let text = narrow
        .iter()
        .map(|span| span.content.as_str())
        .collect::<String>();
    assert_eq!(text.chars().count(), 10);
    assert!(!text.contains("…"));
}

use super::*;
use crate::theme::{ColorMode, Theme};
use serde_json::json;

/// `count` placeholder detail items for one dock group.
fn items(count: usize) -> Vec<String> {
    (1..=count).map(|item| format!("item {item}")).collect()
}

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

#[test]
fn token_count_formats() {
    assert_eq!(format_token_count(999), "999");
    assert_eq!(format_token_count(6_123), "6.1k");
    assert_eq!(format_token_count(61_234), "61k");
    assert_eq!(format_token_count(1_234_567), "1.2M");
}

#[test]
fn splash_renders_logo_version_model_and_cwd() {
    let state = ChromeState {
        version: "0.0.0".to_string(),
        cwd: "/tmp/project".to_string(),
        model_id: Some("faux-1".to_string()),
        ..Default::default()
    };
    let lines = render_splash(&state, &theme(), 120);
    assert_eq!(lines[0], Vec::new());
    let text = |line: &Line| line.iter().map(|s| s.content.as_str()).collect::<String>();
    assert!(lines.iter().any(|l| text(l).contains("prime agent v0.0.0")));
    assert!(lines.iter().any(|l| text(l).contains("model faux-1")));
    assert!(lines.iter().any(|l| text(l).contains("cwd /tmp/project")));
    assert!(lines
        .iter()
        .any(|l| text(l).contains("\u{2597}\u{2584}\u{2584}")));
}

#[test]
fn top_bar_centers_name_with_cost() {
    let state = ChromeState {
        chat_name: "shared-cwd".to_string(),
        cost_usd: Some(0.0),
        ..Default::default()
    };
    let line = render_top_bar(&state, &theme(), 120);
    let text = line.iter().map(|s| s.content.as_str()).collect::<String>();
    assert!(text.contains("shared-cwd  $0.00"));
    let start = text.find("shared-cwd").unwrap();
    assert_eq!(start, 55);
}

#[test]
fn tray_left_and_right_labels() {
    let state = ChromeState {
        show_manage: true,
        model_id: Some("faux-1".to_string()),
        context: Some(ContextUsage {
            tokens: 6_123,
            context_window: 128_000,
        }),
        ..Default::default()
    };
    let line = render_tray(&state, &theme(), 120);
    let text = line.iter().map(|s| s.content.as_str()).collect::<String>();
    assert!(text.starts_with("\u{2190} manage"));
    assert!(text.contains("6.1k/128k 5% \u{00b7} faux-1"));
    assert_eq!(str_width(&text), 120);
}

/// A model on this machine or a private network shows `⌂` and the success tone in the tray,
/// read from its provider and base URL alone; a public endpoint keeps the dim label.
#[test]
fn a_local_model_is_marked_in_the_tray() {
    let hosts: Vec<ModelHost> = [
        ("mock", "http://127.0.0.1:18797/v1"),
        ("custom", "http://localhost:8080/v1"),
        ("custom", "http://192.168.1.20:1234/v1"),
        ("custom", "http://10.0.0.5/v1"),
        ("custom", "http://172.20.0.2/v1"),
        ("custom", "http://[::1]:11434/v1"),
        ("custom", "http://gpu-box.local:8000/v1"),
        ("ollama", "https://ollama.example.com/v1"),
        ("openai", "https://api.openai.com/v1"),
        ("custom", "http://172.32.0.1/v1"),
        ("custom", "not a url"),
    ]
    .iter()
    .map(|(provider, base_url)| ModelHost::of(provider, base_url))
    .collect();
    assert_eq!(
        hosts,
        [
            [ModelHost::Local; 8].as_slice(),
            [ModelHost::Remote; 3].as_slice()
        ]
        .concat()
    );
    let theme = theme();
    let state = ChromeState {
        model_id: Some("mock-1".to_string()),
        model_host: ModelHost::of("mock", "http://127.0.0.1:18797/v1"),
        ..Default::default()
    };
    let line = render_tray(&state, &theme, 60);
    let model: Vec<(String, Option<ratatui::style::Color>)> = line
        .iter()
        .skip_while(|span| span.content.trim().is_empty())
        .map(|span| (span.content.clone(), span.style.fg))
        .collect();
    let success = theme.fg_style(ThemeColor::Success).fg;
    assert_eq!(
        model,
        vec![
            ("\u{2302} ".to_string(), success),
            ("mock-1".to_string(), success)
        ]
    );
}

/// `fast` for the priority tier, the tier name for any other non-default
/// tier, nothing for `default` or an unset tier.
#[test]
fn tray_service_tier_badge_follows_the_ts_shape() {
    let state = |tier: Option<&str>| ChromeState {
        model_id: Some("gpt-5.5".to_string()),
        service_tier: tier.map(str::to_string),
        ..Default::default()
    };
    let badge = |tier: Option<&str>| {
        let line = render_tray(&state(tier), &theme(), 120);
        line.iter().map(|s| s.content.as_str()).collect::<String>()
    };
    assert!(
        badge(Some("priority")).contains("gpt-5.5 \u{00b7} fast"),
        "priority renders the fast token"
    );
    assert!(
        badge(Some("flex")).contains("gpt-5.5 \u{00b7} flex"),
        "a non-default tier renders its name"
    );
    for tier in [Some("default"), None] {
        let text = badge(tier);
        assert!(!text.contains(" \u{00b7} "), "{tier:?} renders no badge");
    }
}

/// A level on a reasoning model renders; everything else keeps the bare id.
#[test]
fn effort_suffix_gates_on_model_reasoning() {
    let reasoning = json!({
        "model": { "id": "faux-1", "provider": "faux", "reasoning": true },
        "thinkingLevel": "high",
    });
    assert_eq!(
        tray_thinking_suffix(&reasoning),
        Some("high".to_string()),
        "a reasoning model's session level renders as the suffix"
    );
    let plain = json!({
        "model": { "id": "faux-1", "provider": "faux", "reasoning": false },
        "thinkingLevel": "high",
    });
    assert_eq!(
        tray_thinking_suffix(&plain),
        None,
        "no reasoning, no suffix"
    );
    assert_eq!(
        tray_thinking_suffix(&json!({ "thinkingLevel": "high" })),
        None,
        "no model block, no suffix"
    );
    let unknown = json!({
        "model": { "id": "faux-1", "provider": "faux", "reasoning": true },
        "thinkingLevel": "default",
    });
    assert_eq!(
        tray_thinking_suffix(&unknown),
        None,
        "a level outside the wire vocabulary keeps the bare id"
    );
    let off = json!({
        "model": { "id": "faux-1", "provider": "faux", "reasoning": true },
        "thinkingLevel": "off",
    });
    assert_eq!(
        tray_thinking_suffix(&off),
        Some("off".to_string()),
        "an explicit off renders `model:off` like the TS tray"
    );
}

#[test]
fn tray_renders_the_effort_suffix_and_the_bare_id_without_it() {
    let mut state = ChromeState {
        show_manage: true,
        model_id: Some("faux-1".to_string()),
        thinking_suffix: Some("high".to_string()),
        ..Default::default()
    };
    let line = render_tray(&state, &theme(), 120);
    let text = line.iter().map(|s| s.content.as_str()).collect::<String>();
    assert!(
        text.contains("\u{25cf} faux-1:high"),
        "the level's glyph and suffix ride the id: {text}"
    );
    let glyph = line
        .iter()
        .find(|s| s.content == "\u{25cf} ")
        .map(|s| s.style);
    assert_eq!(glyph, Some(theme().fg_style(ThemeColor::ThinkingHigh)));
    state.thinking_suffix = None;
    let line = render_tray(&state, &theme(), 120);
    let text = line.iter().map(|s| s.content.as_str()).collect::<String>();
    assert!(text.contains("faux-1"), "the bare id still renders: {text}");
    assert!(!text.contains('\u{25cf}'), "no level, no glyph: {text}");
    assert!(!text.contains("faux-1:"), "no suffix, no colon: {text}");
}

/// The tray never carries the goal label (operator directive 2026-09-24: "pursuing goal
/// should not show up in the line below prompt bar"); the goal lives in the activity dock.
#[test]
fn tray_never_repeats_the_heartbeat_counts() {
    let state = ChromeState {
        show_manage: true,
        model_id: Some("mock-1".to_string()),
        context: Some(ContextUsage {
            tokens: 190,
            context_window: 128_000,
        }),
        ..Default::default()
    };
    let line = render_tray(&state, &theme(), 120);
    let text = line.iter().map(|s| s.content.as_str()).collect::<String>();
    assert!(text.contains("190/128k 0% \u{b7} mock-1"));
    assert!(!text.contains("Pursuing goal"));
    assert!(!text.contains("goal"));
    assert!(!text.contains("heartbeat"));
    assert!(!text.contains("Ctrl+R"));
    assert_eq!(str_width(&text), 120);
}

/// The gap between the lanes is the context gauge: the used share in `━` (accent, amber from
/// 70%, red from 90%), the rest in `─`, a space at each end.
#[test]
fn tray_gap_is_the_context_gauge() {
    let gauge = |tokens: u64| {
        let state = ChromeState {
            show_manage: true,
            context: Some(ContextUsage {
                tokens,
                context_window: 200_000,
            }),
            ..Default::default()
        };
        let line = render_tray(&state, &theme(), 60);
        assert_eq!(
            str_width(&line.iter().map(|s| s.content.as_str()).collect::<String>()),
            60
        );
        let span = |glyph: char| line.iter().find(|s| s.content.starts_with(glyph)).cloned();
        let used = span('\u{2501}').map_or(0, |s| s.content.chars().count());
        let free = span('\u{2500}').map_or(0, |s| s.content.chars().count());
        (used, free, span('\u{2501}').map(|s| s.style))
    };
    let t = theme();
    // `← manage` (8) + `100k/200k 50%` (13) leave 39 cells: 37 gauge cells, half used.
    assert_eq!(
        gauge(100_000),
        (19, 18, Some(t.fg_style(ThemeColor::Accent)))
    );
    assert_eq!(gauge(150_000).2, Some(t.fg_style(ThemeColor::Warning)));
    assert_eq!(gauge(190_000).2, Some(t.fg_style(ThemeColor::Error)));
}

#[test]
fn tray_override_replaces_location_label() {
    let state = ChromeState {
        show_manage: true,
        tray_override: Some("Press Ctrl+C again to exit".to_string()),
        model_id: Some("faux-1".to_string()),
        ..Default::default()
    };
    let line = render_tray(&state, &theme(), 120);
    let text = line.iter().map(|s| s.content.as_str()).collect::<String>();
    assert!(text.starts_with("Press Ctrl+C again to exit"));
    assert!(!text.contains("manage"));
}

#[test]
fn detail_status_label() {
    assert_eq!(
        conversation_detail_status(false, false, "Ctrl+O"),
        "Collapsed mode (Ctrl+O to expand)"
    );
    assert_eq!(
        conversation_detail_status(false, true, "Ctrl+O"),
        "Details mode (Ctrl+O to expand)"
    );
    assert_eq!(
        conversation_detail_status(true, true, "Ctrl+O"),
        "Expanded mode (Ctrl+O to collapse)"
    );
}

/// The collapsed shells segment breaks out its persistent shells like the heartbeats' paused
/// cluster, and its click region covers the cluster; without any it keeps the plain count.
#[test]
fn the_collapsed_shells_segment_counts_persistent_shells() {
    let render = |shells_persistent| {
        let dock = ActivityDock {
            shells: items(2),
            shells_persistent,
            ..ActivityDock::default()
        };
        let (frame, segments) = render_activity_dock_segments(&dock, &theme(), 120);
        let text: String = frame[1].iter().map(|span| span.content.as_str()).collect();
        let bash = segments
            .iter()
            .find(|segment| segment.group == ActivityGroup::Bash)
            .expect("the shells segment")
            .cols
            .clone();
        text.chars()
            .skip(bash.start)
            .take(bash.end - bash.start)
            .collect::<String>()
    };
    assert_eq!(render(1), "▸ 2 shells · 1 persistent");
    assert_eq!(render(0), "▸ 2 shells");
}

/// A session's accent is stable per name, differs across names, keeps its hue in 150°-294°
/// (never the gauge's amber or red), and falls back to the theme accent without a name or a
/// true-color accent.
#[test]
fn the_session_accent_is_a_stable_hue_per_name() {
    use ratatui::style::Color;
    let t = theme();
    let rgb = |name: &str| match session_accent(name, &t).fg {
        Some(Color::Rgb(r, g, b)) => (r, g, b),
        other => panic!("{other:?}"),
    };
    assert_eq!(rgb("login-fix"), rgb("login-fix"));
    assert_ne!(rgb("login-fix"), rgb("refactor-auth"));
    for name in [
        "a",
        "b",
        "login-fix",
        "refactor-auth",
        "prime",
        "docs",
        "zz",
    ] {
        let (r, g, b) = rgb(name);
        // In the 150°-294° band red never leads both other channels (that is 330°-30°).
        assert!(
            !(r > g && r > b && g >= b),
            "{name}: {r},{g},{b} reads as red or amber"
        );
    }
    assert_eq!(session_accent("", &t), t.fg_style(ThemeColor::Accent));
    let ansi = Theme::builtin("prime", crate::theme::ColorMode::Color256);
    assert_eq!(
        session_accent("login-fix", &ansi),
        ansi.fg_style(ThemeColor::Accent)
    );
}

/// The gauge ticks the auto-compaction threshold with `┃`; auto-compaction off draws none.
#[test]
fn the_gauge_ticks_the_compaction_threshold() {
    let gauge = |compaction_percent: Option<f64>| {
        let state = ChromeState {
            show_manage: true,
            context: Some(ContextUsage {
                tokens: 50_000,
                context_window: 200_000,
            }),
            compaction_percent,
            ..Default::default()
        };
        render_tray(&state, &theme(), 50)
            .iter()
            .map(|s| s.content.as_str())
            .collect::<String>()
    };
    // `← manage` (8) + `50k/200k 25%` (12) leave 30 cells: 28 gauge cells, 7 used, the tick at 80%.
    assert_eq!(
        gauge(Some(80.0)),
        format!(
            "\u{2190} manage {}{}\u{2503}{} 50k/200k 25%",
            "\u{2501}".repeat(7),
            "\u{2500}".repeat(15),
            "\u{2500}".repeat(5)
        )
    );
    assert!(!gauge(None).contains('\u{2503}'));
}

/// As the width drops, the tray gives segments up in its stated order (gauge, tier, depth,
/// tokens/window, effort with its glyph, `manage`, percent, model) — each row fits its width, no segment is cut
/// mid-word, and the lanes never touch.
#[test]
fn the_tray_degrades_in_its_shrink_order() {
    let state = ChromeState {
        show_manage: true,
        tray_depth: Some(2),
        context: Some(ContextUsage {
            tokens: 50_000,
            context_window: 200_000,
        }),
        model_id: Some("claude-sonnet-4-5".to_string()),
        thinking_suffix: Some("high".to_string()),
        service_tier: Some("priority".to_string()),
        ..Default::default()
    };
    let rows: Vec<String> = [140, 80, 60, 50, 45, 40, 35, 30, 25, 20, 12, 4]
        .into_iter()
        .map(|width| {
            let line = render_tray(&state, &theme(), width);
            let text: String = line.iter().map(|s| s.content.as_str()).collect();
            assert!(str_width(&text) <= width, "{width}: {text}");
            // The gauge is not text: collapse it so the rows read as their segments.
            text.chars()
                .filter(|c| !matches!(c, '\u{2501}' | '\u{2500}' | '\u{2503}'))
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let full = "\u{2190} manage depth 2 50k/200k 25% \u{b7} \u{25cf} claude-sonnet-4-5:high";
    assert_eq!(
        rows,
        [
            // 140, 80: everything (the gauge takes the slack).
            format!("{full} \u{b7} fast"),
            format!("{full} \u{b7} fast"),
            // 60: the gauge is gone, then the tier badge.
            full.to_string(),
            // 50: the subagent depth.
            "\u{2190} manage 50k/200k 25% \u{b7} \u{25cf} claude-sonnet-4-5:high".to_string(),
            // 45, 40: the context label keeps its percent.
            "\u{2190} manage 25% \u{b7} \u{25cf} claude-sonnet-4-5:high".to_string(),
            "\u{2190} manage 25% \u{b7} \u{25cf} claude-sonnet-4-5:high".to_string(),
            // 35: the effort suffix, with its thinking glyph.
            "\u{2190} manage 25% \u{b7} claude-sonnet-4-5".to_string(),
            // 30, 25: the `manage` word (the arrow stays).
            "\u{2190} 25% \u{b7} claude-sonnet-4-5".to_string(),
            "\u{2190} 25% \u{b7} claude-sonnet-4-5".to_string(),
            // 20: the percent; the model goes last (12, 4).
            "\u{2190} claude-sonnet-4-5".to_string(),
            "\u{2190}".to_string(),
            "\u{2190}".to_string(),
        ]
    );
}

/// The rate-limit reset chip rides the location lane in the session accent; at narrow widths it
/// outlives the context percent and drops before the model label.
#[test]
fn the_reset_chip_renders_and_drops_before_the_model() {
    let state = |chip: Option<&str>| ChromeState {
        show_manage: true,
        chat_name: "fix-login".to_string(),
        context: Some(ContextUsage {
            tokens: 50_000,
            context_window: 200_000,
        }),
        model_id: Some("claude-sonnet-4-5".to_string()),
        reset_chip: chip.map(str::to_string),
        ..Default::default()
    };
    let row = |chip: Option<&str>, width: usize| {
        let line = render_tray(&state(chip), &theme(), width);
        let text: String = line.iter().map(|s| s.content.as_str()).collect();
        assert!(str_width(&text) <= width, "{width}: {text}");
        text.chars()
            .filter(|c| !matches!(c, '\u{2501}' | '\u{2500}' | '\u{2503}'))
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let chip = Some("limit resets in 1h 2m");
    assert_eq!(
        [
            row(None, 80),
            row(chip, 80),
            row(None, 44),
            row(chip, 44),
            row(chip, 30)
        ],
        [
            "\u{2190} manage 50k/200k 25% \u{b7} claude-sonnet-4-5",
            "\u{2190} manage limit resets in 1h 2m 50k/200k 25% \u{b7} claude-sonnet-4-5",
            "\u{2190} manage 50k/200k 25% \u{b7} claude-sonnet-4-5",
            // 44: the `manage` word and the context label go first; the chip stays with the model.
            "\u{2190} limit resets in 1h 2m claude-sonnet-4-5",
            // 30: the chip goes before the model.
            "\u{2190} claude-sonnet-4-5",
        ]
        .map(str::to_string)
    );
    let line = render_tray(&state(chip), &theme(), 80);
    let chip_span = line
        .iter()
        .find(|span| span.content == "limit resets in 1h 2m")
        .expect("the chip renders as one span");
    assert_eq!(
        chip_span.style,
        session_accent("fix-login", &theme()),
        "the chip wears the session accent"
    );
}
