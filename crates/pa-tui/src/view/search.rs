//! Transcript search (`app.transcript.search`): a one-row bar in the dock over the conversation.
//! Typing filters incrementally, every match on screen is lit and the current one stands out,
//! and the next/previous keys move the window to a match. The bar searches what the client has:
//! each loaded entry's rows as they render with everything shown (a collapsed card's output, a
//! folded thought), so a match the collapsed view hides still counts, and the current match's
//! entry opens while it is current. Esc puts the window and every opened entry back; Enter
//! closes at the match.
//!
//! The index is built a slice at a time (`SLICE_BUDGET`), nearest entries first, so a long
//! transcript never holds a frame: the run loop's tick and each key run one slice.

use super::search_text::{fold, paint_matches, visible_text, Query};
use super::AgentView;
use crate::chat::Detail;
use crate::menu_chrome::{EscVerb, Footer, HintRank, ELLIPSIS, SEPARATOR};
use crate::search_input::SearchInput;
use crate::theme::{ThemeBg, ThemeColor};
use crate::width::{str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};
use ratatui::style::{Modifier, Style};
use std::time::{Duration, Instant};

/// The work one slice of the index may do before it yields to the frame.
const SLICE_BUDGET: Duration = Duration::from_millis(8);
/// The empty bar's placeholder.
const PLACEHOLDER: &str = "Search the conversation";
/// The columns the query keeps before the status and hints take the rest of the row.
const QUERY_MIN_COLUMNS: usize = 12;

/// Whether the transcript the client holds is the whole session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryScope {
    /// Every row of the session is loaded.
    Complete,
    /// Older pages live only in the daemon: the bar says it searched the loaded history.
    LoadedOnly,
}

/// How a search closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchOutcome {
    /// Enter at a match: the window stays there.
    Jumped,
    /// Esc, or Enter with nothing found: the window went back to where it was.
    Cancelled,
    /// Closed while the query matched nothing.
    NoMatch,
}

/// What a closed search reports (`tui transcript search`): counts and enums only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SearchSummary {
    pub(crate) outcome: SearchOutcome,
    pub(crate) steps: u64,
    pub(crate) revealed: bool,
    pub(crate) history: HistoryScope,
}

/// What one key did to the open search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchKey {
    /// The query, the current match or nothing changed; the bar stays open.
    Handled,
    /// The bar closed. `to_bottom`: the window went back to following the tail.
    Closed {
        summary: SearchSummary,
        to_bottom: bool,
    },
}

/// One match: a row of an entry's search rows and the match's first byte in that row's visible
/// text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    row: u32,
    start: u32,
}

/// One entry's rows as visible text, joined by newlines, with the row starts.
struct EntryText {
    text: String,
    /// The case fold, when it differs from `text` (same byte offsets).
    folded: Option<String>,
    row_starts: Vec<usize>,
    /// The rows were rendered with the entry opened (it shows less right now): landing on one of
    /// its matches opens it.
    opened: bool,
}

struct Slot {
    text: EntryText,
    /// The matches of query generation `.0`.
    hits: Option<(u64, Vec<Hit>)>,
}

/// Where the window was when the bar opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Bottom,
    /// A row of the splash or the older-history marker above the first entry.
    Head(usize),
    Entry {
        index: usize,
        row: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Current {
    entry: usize,
    hit: Hit,
}

/// The open search bar's state.
pub(crate) struct TranscriptSearch {
    input: SearchInput,
    query: Option<Query>,
    /// Bumped at each query change: a slot's hits of an older generation are stale.
    generation: u64,
    origin: Origin,
    /// The last entry the window showed when the bar opened: the first match picked is the
    /// nearest at or above it, and the index grows outward from it.
    anchor: usize,
    /// The width the slots' rows were rendered at.
    width: usize,
    slots: Vec<Option<Slot>>,
    current: Option<Current>,
    /// The entry the search opened for the current match, to close again on leaving it.
    opened: Option<usize>,
    /// The current match changed before every height was cached: the window moves to it once
    /// the slices have measured the rest.
    landing_owed: bool,
    steps: u64,
    opened_any: bool,
    history: HistoryScope,
}

impl TranscriptSearch {
    /// Whether the index or the current query's matches still have work.
    fn pending(&self) -> bool {
        self.landing_owed
            || (0..self.slots.len())
                .any(|index| self.slots[index].is_none() || self.slot_hits_stale(index))
    }

    /// Whether an indexed entry's matches are of an older query.
    fn slot_hits_stale(&self, index: usize) -> bool {
        self.query.is_some()
            && self.slots[index].as_ref().is_some_and(|slot| {
                slot.hits
                    .as_ref()
                    .is_none_or(|(generation, _)| *generation != self.generation)
            })
    }

    fn hits(&self, entry: usize) -> &[Hit] {
        match self.slots.get(entry) {
            Some(Some(Slot {
                hits: Some((generation, hits)),
                ..
            })) if *generation == self.generation => hits,
            Some(Some(_) | None) | None => &[],
        }
    }

    /// The matches found so far and the current one's ordinal among them (1-based).
    fn counter(&self) -> (usize, Option<usize>) {
        let total = (0..self.slots.len())
            .map(|entry| self.hits(entry).len())
            .sum();
        let ordinal = self.current.and_then(|current| {
            let before: usize = (0..current.entry).map(|entry| self.hits(entry).len()).sum();
            let within = self
                .hits(current.entry)
                .iter()
                .position(|hit| *hit == current.hit)?;
            Some(before + within + 1)
        });
        (total, ordinal)
    }

    /// The nearest match at or above the anchor, else the nearest below it, among the entries
    /// indexed without a gap from the anchor.
    fn nearest(&self) -> Option<Current> {
        let anchor = self.anchor.min(self.slots.len().checked_sub(1)?);
        for entry in (0..=anchor).rev() {
            self.slots.get(entry)?.as_ref()?;
            if let Some(hit) = self.hits(entry).last() {
                return Some(Current { entry, hit: *hit });
            }
        }
        for entry in anchor + 1..self.slots.len() {
            self.slots.get(entry)?.as_ref()?;
            if let Some(hit) = self.hits(entry).first() {
                return Some(Current { entry, hit: *hit });
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepDirection {
    Next,
    Previous,
}

/// What a key means to the open bar, read through the effective bindings.
enum KeyRole {
    Cancel,
    Confirm,
    Step(StepDirection),
    Edit,
}

/// Hand one closed search's report to the telemetry seam and return at once: the key path
/// never waits on the delivery.
pub(crate) fn report_closed(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    summary: SearchSummary,
) {
    let report = crate::interactive::TranscriptSearchReport {
        outcome: match summary.outcome {
            SearchOutcome::Jumped => "jumped",
            SearchOutcome::Cancelled => "cancelled",
            SearchOutcome::NoMatch => "no_match",
        },
        steps: summary.steps,
        revealed: summary.revealed,
        history: match summary.history {
            HistoryScope::Complete => "complete",
            HistoryScope::LoadedOnly => "loaded_only",
        },
    };
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry.transcript_searched(report).await;
        });
    }
}

// Every entry point below measures and renders rows the way a fullscreen frame does (the image
// fallback on), so the layout caches it fills and reads are the frame's own: a measure outside
// that scope would differ in its options and clear them.
impl AgentView {
    /// Open the bar over the transcript the view holds, remembering where the window is.
    pub(crate) fn open_transcript_search(&mut self, history: HistoryScope) {
        let origin = if self.following || self.layout_width == 0 {
            Origin::Bottom
        } else if let Some((section, row)) = self.window_top_cursor() {
            // The last frame's top, read without measuring the transcript.
            match section {
                0 => Origin::Head(row),
                section => Origin::Entry {
                    index: section - 1,
                    row,
                },
            }
        } else {
            self.resolve_sparse_geometry();
            let layout = crate::image_component::with_fullscreen_image_fallback(|| {
                self.layout_pass(self.layout_width)
            });
            match layout.cursor_at(self.scroll_top) {
                (0, row) => Origin::Head(row),
                (section, row) => Origin::Entry {
                    index: section - 1,
                    row,
                },
            }
        };
        let anchor = self
            .visible_entries()
            .map_or(self.chat.len().saturating_sub(1), |(_, last)| last);
        self.transcript_search = Some(TranscriptSearch {
            input: SearchInput::new(),
            query: None,
            generation: 0,
            origin,
            anchor,
            width: self.layout_width,
            slots: std::iter::repeat_with(|| None)
                .take(self.chat.len())
                .collect(),
            current: None,
            opened: None,
            landing_owed: false,
            steps: 0,
            opened_any: false,
            history,
        });
    }

    /// The open bar's history scope follows the transcript window's.
    pub(crate) fn set_transcript_search_history(&mut self, history: HistoryScope) {
        if let Some(search) = self.transcript_search.as_mut() {
            search.history = history;
        }
    }

    /// Whether the open search still has index or match work for the run loop's tick.
    pub(crate) fn transcript_search_pending(&self) -> bool {
        self.transcript_search.as_ref().is_some_and(|search| {
            search.pending()
                || (self.layout_width > 0
                    && (0..self.chat.len()).any(|index| self.search_height_missing(index)))
        })
    }

    /// One slice of the open search's work (the run loop's tick); whether anything changed.
    pub(crate) fn continue_transcript_search(&mut self) -> bool {
        let Some(mut search) = self.transcript_search.take() else {
            return false;
        };
        let changed = crate::image_component::with_fullscreen_image_fallback(|| {
            self.search_slice(&mut search)
        });
        self.transcript_search = Some(search);
        changed
    }

    /// One key while the bar is open: Esc (or Ctrl+C) closes and puts the window back, Enter
    /// closes at the match, the step keys move between matches, and every other key edits the
    /// query.
    pub(crate) fn transcript_search_key(&mut self, key: &str) -> Option<SearchKey> {
        crate::image_component::with_fullscreen_image_fallback(|| {
            let mut search = self.transcript_search.take()?;
            let role = {
                let kb = self.editor.keybindings();
                if key == "ctrl+c" || kb.matches(key, "tui.select.cancel") {
                    KeyRole::Cancel
                } else if kb.matches(key, "tui.select.confirm") {
                    KeyRole::Confirm
                } else if kb.matches(key, "app.transcript.searchNext") {
                    KeyRole::Step(StepDirection::Next)
                } else if kb.matches(key, "app.transcript.searchPrevious") {
                    KeyRole::Step(StepDirection::Previous)
                } else {
                    KeyRole::Edit
                }
            };
            match role {
                KeyRole::Cancel | KeyRole::Confirm => {
                    let (total, _) = search.counter();
                    let outcome = if matches!(role, KeyRole::Confirm) && search.current.is_some() {
                        SearchOutcome::Jumped
                    } else if search.query.is_some() && total == 0 {
                        SearchOutcome::NoMatch
                    } else {
                        SearchOutcome::Cancelled
                    };
                    let to_bottom = match outcome {
                        SearchOutcome::Jumped => false,
                        SearchOutcome::Cancelled | SearchOutcome::NoMatch => {
                            self.search_close_opened(&mut search);
                            self.search_restore_origin(search.origin);
                            search.origin == Origin::Bottom
                        }
                    };
                    return Some(SearchKey::Closed {
                        summary: SearchSummary {
                            outcome,
                            steps: search.steps,
                            revealed: search.opened_any,
                            history: search.history,
                        },
                        to_bottom,
                    });
                }
                KeyRole::Step(direction) => {
                    // The match after or before the current one in transcript order, wrapping at
                    // the ends; with none current, the nearest one.
                    let all: Vec<Current> = (0..search.slots.len())
                        .flat_map(|entry| {
                            search
                                .hits(entry)
                                .iter()
                                .map(move |hit| Current { entry, hit: *hit })
                        })
                        .collect();
                    let at = search
                        .current
                        .and_then(|current| all.iter().position(|hit| *hit == current));
                    let next = match (direction, at) {
                        (StepDirection::Next, Some(at)) => all.get((at + 1) % all.len()).copied(),
                        (StepDirection::Previous, Some(at)) => {
                            all.get((at + all.len() - 1) % all.len()).copied()
                        }
                        (StepDirection::Next | StepDirection::Previous, None) => {
                            search.nearest().or_else(|| all.first().copied())
                        }
                    };
                    if let Some(next) = next {
                        search.steps += 1;
                        search.current = Some(next);
                        self.search_land(&mut search);
                    }
                }
                KeyRole::Edit => {
                    let before = search.input.value().to_string();
                    search.input.handle_key(key, self.editor.keybindings());
                    if search.input.value() != before {
                        self.search_requery(&mut search);
                    }
                }
            }
            self.transcript_search = Some(search);
            Some(SearchKey::Handled)
        })
    }

    /// A paste into the open bar edits the query.
    pub(crate) fn transcript_search_paste(&mut self, text: &str) {
        let Some(mut search) = self.transcript_search.take() else {
            return;
        };
        let before = search.input.value().to_string();
        search.input.paste(text);
        if search.input.value() != before {
            crate::image_component::with_fullscreen_image_fallback(|| {
                self.search_requery(&mut search);
            });
        }
        self.transcript_search = Some(search);
    }

    fn search_requery(&mut self, search: &mut TranscriptSearch) {
        search.query = Query::new(search.input.value());
        search.generation += 1;
        search.current = None;
        self.search_close_opened(search);
        self.search_slice(search);
        if search.current.is_none() {
            // Nothing to show yet: the window goes back to where the search started.
            self.search_restore_origin(search.origin);
        }
    }

    /// Index and match entries nearest the anchor first until the slice budget is spent, then
    /// pick and land on the first match when none is current. Whether anything changed.
    fn search_slice(&mut self, search: &mut TranscriptSearch) -> bool {
        if self.layout_width == 0 {
            return false;
        }
        if search.width != self.layout_width {
            // A resize lays every row out again: the index restarts at the new width.
            self.search_close_opened(search);
            search.width = self.layout_width;
            search.slots.iter_mut().for_each(|slot| *slot = None);
            search.current = None;
        }
        // The frame's own layout preparation: the per-entry caches cover every entry.
        self.prepare_layout(self.layout_width);
        let len = self.chat.len();
        search.slots.resize_with(len, || None);
        let started = Instant::now();
        let anchor = search.anchor.min(len.saturating_sub(1));
        let order: Vec<usize> = if len == 0 {
            Vec::new()
        } else {
            (0..=anchor).rev().chain(anchor + 1..len).collect()
        };
        let width = self.layout_width;
        let mut changed = false;
        for index in order {
            let height_missing = self.search_height_missing(index);
            // The entry's height as the window lays it out, when this visit renders that.
            let mut shown_height = None;
            if search.slots[index].is_none() {
                // The entry's search rows: as they render opened (tool output expanded,
                // thinking shown) when it shows less now, else as the window shows them. An
                // entry still changing (a streaming reply, a running tool) is read as it shows.
                let opened = Self::entry_cacheable(&self.chat[index])
                    && self.entry_detail(index) != Detail::All;
                let rows = if opened {
                    // The open render flips the entry's own expansion for this one render; the
                    // layout caches never see it.
                    flip_card(&mut self.toggled_cards, index);
                    let preceded = index > 0 && Self::is_compact_neighbor(&self.chat[index - 1]);
                    let rows =
                        self.render_entry(index, &self.chat[index], width, index == 0, preceded);
                    flip_card(&mut self.toggled_cards, index);
                    rows
                } else {
                    let rows = self.sparse_entry_rows(index, width);
                    shown_height = Some(rows.len());
                    rows.range(0, rows.len())
                };
                let mut text = String::new();
                let mut row_starts = Vec::with_capacity(rows.len());
                for (row, line) in rows.iter().enumerate() {
                    if row > 0 {
                        text.push('\n');
                    }
                    row_starts.push(text.len());
                    text.push_str(&visible_text(line));
                }
                let folded = match fold(&text) {
                    std::borrow::Cow::Borrowed(_) => None,
                    std::borrow::Cow::Owned(folded) => Some(folded),
                };
                search.slots[index] = Some(Slot {
                    text: EntryText {
                        text,
                        folded,
                        row_starts,
                        opened,
                    },
                    hits: None,
                });
            } else if !search.slot_hits_stale(index) && !height_missing {
                continue;
            }
            if let (Some(query), Some(slot)) = (&search.query, search.slots[index].as_mut()) {
                let text = &slot.text;
                let folded = text.folded.as_deref().unwrap_or(&text.text);
                let hits = query
                    .find(&text.text, folded)
                    .into_iter()
                    .map(|(start, _)| {
                        let row = text
                            .row_starts
                            .partition_point(|row_start| *row_start <= start)
                            .saturating_sub(1);
                        let row_start = text.row_starts.get(row).copied().unwrap_or_default();
                        Hit {
                            row: u32::try_from(row).unwrap_or(u32::MAX),
                            start: u32::try_from(start - row_start).unwrap_or(u32::MAX),
                        }
                    })
                    .collect();
                slot.hits = Some((search.generation, hits));
            }
            // The entry's height at the current level goes into the layout's cache, so landing
            // on a match never measures the whole transcript on the key path.
            if height_missing {
                let entry = &self.chat[index];
                let preceded = index > 0 && Self::is_compact_neighbor(&self.chat[index - 1]);
                let spacing = self.entry_spacing(index, entry, index == 0, preceded);
                let count = match shown_height {
                    Some(count) => count,
                    None => self.count_entry_rows(index, width),
                };
                self.entry_heights[index][detail_slot(self.detail)] = Some((spacing, count));
            }
            changed = true;
            if started.elapsed() >= SLICE_BUDGET {
                break;
            }
        }
        if search.current.is_none() && search.query.is_some() {
            if let Some(nearest) = search.nearest() {
                search.current = Some(nearest);
                search.landing_owed = true;
            }
        }
        if search.landing_owed {
            self.search_land(search);
            changed = true;
        }
        changed
    }

    /// Whether a settled entry's height at the current level is not cached yet.
    fn search_height_missing(&self, index: usize) -> bool {
        Self::entry_cacheable(&self.chat[index])
            && self
                .entry_heights
                .get(index)
                .is_some_and(|heights| heights[detail_slot(self.detail)].is_none())
    }

    /// Close the entry the search opened for its last current match.
    fn search_close_opened(&mut self, search: &mut TranscriptSearch) {
        if let Some(index) = search.opened.take() {
            if index < self.chat.len() {
                self.toggle_card_expansion(index);
            }
        }
    }

    /// Show the current match: open its entry when the match is in rows it hides (closing the
    /// one opened before), and move the window only when the match row is not already in view
    /// clear of the pinned prompt and the follow pill, putting it a third of the way down.
    fn search_land(&mut self, search: &mut TranscriptSearch) {
        let Some(current) = search.current else {
            return;
        };
        // Until every settled entry's height is cached the landing waits for the index slices:
        // measuring the rest here would hold the key.
        search.landing_owed = (0..self.chat.len()).any(|index| self.search_height_missing(index));
        if search.landing_owed {
            return;
        }
        if search.opened != Some(current.entry) {
            self.search_close_opened(search);
            let hidden = search.slots[current.entry]
                .as_ref()
                .is_some_and(|slot| slot.text.opened)
                && self.entry_detail(current.entry) != Detail::All;
            if hidden {
                self.toggle_card_expansion(current.entry);
                search.opened = Some(current.entry);
                search.opened_any = true;
            }
        }
        if self.layout_width == 0 {
            return;
        }
        let was_following = self.following;
        self.resolve_sparse_geometry();
        let layout = self.layout_pass(self.layout_width);
        let max_scroll = layout.total.saturating_sub(self.window_rows);
        let top = if was_following {
            max_scroll
        } else {
            self.scroll_top.min(max_scroll)
        };
        let target = layout.entry_start(current.entry) + current.hit.row as usize;
        let margin = usize::from(self.window_rows >= 3);
        let visible = target >= top + margin && target + margin < top + self.window_rows;
        if visible {
            return;
        }
        let top = target.saturating_sub(self.window_rows / 3);
        if top >= max_scroll {
            self.scroll_to_bottom();
            return;
        }
        if was_following {
            self.unseen_entries = 0;
        }
        self.sparse_window = None;
        self.scroll_top = top;
        self.following = false;
    }

    /// Put the window back where the search started.
    fn search_restore_origin(&mut self, origin: Origin) {
        match origin {
            Origin::Bottom => self.scroll_to_bottom(),
            Origin::Head(row) => {
                self.sparse_window = None;
                self.scroll_top = row;
                self.following = self.chat.is_empty();
            }
            Origin::Entry { index, row } => self.scroll_to_entry(index, row),
        }
    }

    /// The splice hook (`splice_chat`, appends, pops, the clear): the open search's entry
    /// indices follow the chat's.
    pub(super) fn search_note_splice(&mut self, range: std::ops::Range<usize>, inserted: usize) {
        let Some(search) = self.transcript_search.as_mut() else {
            return;
        };
        let shift = |index: usize| -> Option<usize> {
            if index < range.start {
                Some(index)
            } else if index < range.end {
                None
            } else {
                Some(index - range.len() + inserted)
            }
        };
        let end = range.end.min(search.slots.len());
        let start = range.start.min(end);
        search
            .slots
            .splice(start..end, std::iter::repeat_with(|| None).take(inserted));
        search.current = search.current.and_then(|current| {
            Some(Current {
                entry: shift(current.entry)?,
                ..current
            })
        });
        search.opened = search.opened.and_then(shift);
        search.anchor = shift(search.anchor).unwrap_or(range.start);
        search.origin = match search.origin {
            Origin::Entry { index, row } => {
                shift(index).map_or(Origin::Bottom, |index| Origin::Entry { index, row })
            }
            Origin::Bottom | Origin::Head(_) => search.origin,
        };
    }

    /// The stale hook (`mark_entry_stale` and its own-rows form): the entry's search
    /// rows are read again.
    pub(super) fn search_note_stale(&mut self, index: usize) {
        if let Some(slot) = self
            .transcript_search
            .as_mut()
            .and_then(|search| search.slots.get_mut(index))
        {
            *slot = None;
        }
    }

    /// The transcript window's rows with the query's matches lit, the current match in its own
    /// style. Only the rows on screen are scanned.
    pub(super) fn search_highlighted(&self, mut rows: Vec<Line>) -> Vec<Line> {
        let Some(search) = &self.transcript_search else {
            return rows;
        };
        let Some(query) = &search.query else {
            return rows;
        };
        let style = self.theme.bg_style(ThemeBg::SelectedBg);
        let current_style = self
            .theme
            .fg_style(ThemeColor::Warning)
            .add_modifier(Modifier::REVERSED);
        let top = self.window_top_cursor();
        for section in &self.click.window_sections {
            // The first visible row's row within its entry: the window's top cursor for the
            // entry the window starts in, the entry's first row otherwise.
            let first = match top {
                Some((cursor_section, row)) if cursor_section == section.entry + 1 => row,
                Some(_) | None => 0,
            };
            for (offset, line) in rows
                .iter_mut()
                .enumerate()
                .take(section.to)
                .skip(section.from)
            {
                let row_in_entry = first + offset - section.from;
                let current = search
                    .current
                    .filter(|current| {
                        current.entry == section.entry && current.hit.row as usize == row_in_entry
                    })
                    .map(|current| current.hit.start as usize);
                *line = paint_matches(line, query, current, style, current_style);
            }
        }
        rows
    }

    /// The bar row: the `/` prompt and the query on the left; the match counter (the scope note
    /// when older history is not loaded) and the step and close keys flush right, the hints
    /// dropping first on a narrow row. Exactly `width` columns.
    pub(super) fn render_search_bar(&self, search: &TranscriptSearch, width: usize) -> Line {
        let theme = &self.theme;
        let kb = self.editor.keybindings();
        let (total, ordinal) = search.counter();
        let pending = search.pending();
        let more = if pending { ELLIPSIS } else { "" };
        let scope = match search.history {
            HistoryScope::Complete => "",
            HistoryScope::LoadedOnly => " in loaded history",
        };
        let status = search.query.as_ref().map(|_| match (total, ordinal) {
            (0, _) if pending => (ThemeColor::Muted, format!("searching{ELLIPSIS}")),
            (0, _) => (ThemeColor::Warning, format!("no match{scope}")),
            (total, Some(ordinal)) => (
                ThemeColor::Muted,
                format!("{ordinal} of {total}{more}{scope}"),
            ),
            (total, None) => (ThemeColor::Muted, format!("{total} found{more}{scope}")),
        });
        let mut footer = Footer::new();
        if total > 0 {
            footer = footer
                .bound(kb, &["app.transcript.searchNext"], "next", HintRank::Useful)
                .bound(
                    kb,
                    &["app.transcript.searchPrevious"],
                    "prev",
                    HintRank::Useful,
                );
        }
        let footer = footer.esc(kb, EscVerb::Close);
        let prompt: Line = vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Accent, "/"),
            Span::raw(" "),
        ];
        let prompt_width = 3;
        let value = search.input.value();
        let query_columns = (str_width(value) + 1).min(QUERY_MIN_COLUMNS.max(width / 3));
        // The right side gets what the prompt, the query and a two-column gap leave (one
        // trailing blank keeps it off the frame edge).
        let budget = width.saturating_sub(prompt_width + query_columns + 3);
        let mut right: Line = Vec::new();
        let mut used = 0;
        if let Some((color, text)) = status {
            let text = truncate_to_width(&text, budget, ELLIPSIS);
            used = str_width(&text);
            right.push(theme.fg_span(color, text));
        }
        let separator = if right.is_empty() {
            0
        } else {
            str_width(SEPARATOR)
        };
        let hints = footer.fitted(budget.saturating_sub(used + separator));
        for (index, hint) in hints.iter().enumerate() {
            if index > 0 || !right.is_empty() {
                right.push(theme.fg_span(ThemeColor::Dim, SEPARATOR));
            }
            right.push(theme.fg_span(ThemeColor::Muted, hint.key.clone()));
            right.push(theme.fg_span(ThemeColor::Dim, format!(" {}", hint.verb)));
        }
        let right_width = crate::width::spans_width(&right);
        let input_columns =
            width.saturating_sub(prompt_width + right_width + usize::from(right_width > 0) * 2 + 1);
        let mut row = prompt;
        if value.is_empty() {
            row.push(Span::styled(
                " ",
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            row.push(theme.fg_span(
                ThemeColor::Dim,
                truncate_to_width(PLACEHOLDER, input_columns.saturating_sub(1), ELLIPSIS),
            ));
        } else {
            // `input_render` keeps the field prompt's two columns out of the width it is given.
            row.extend(crate::menu_panel::input_render(
                theme,
                input_columns + 2,
                value,
                search.input.cursor(),
                true,
            ));
        }
        let row = truncate_line(&row, width.saturating_sub(right_width + 1), "");
        let left_width = crate::width::spans_width(&row);
        let mut row = row;
        row.push(Span::raw(
            " ".repeat(width.saturating_sub(left_width + right_width + 1)),
        ));
        row.extend(right);
        row.push(Span::raw(" "));
        truncate_line(&row, width, "")
    }
}

/// The layout caches' slot for a conversation level.
fn detail_slot(detail: Detail) -> usize {
    match detail {
        Detail::Overview => 0,
        Detail::Details => 1,
        Detail::All => 2,
    }
}

/// Flip one entry's own expansion in place (the open render's bracket).
fn flip_card(toggled: &mut std::collections::BTreeSet<usize>, index: usize) {
    if !toggled.remove(&index) {
        toggled.insert(index);
    }
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
