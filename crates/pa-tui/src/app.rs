//! Terminal app: crossterm event loop driving an [`AgentView`] against a
//! [`SessionStream`]. Both the interactive product surface and the replay
//! verifier binary run through this single loop.

use crate::editor::{Editor, EditorEvent};
use crate::keys::key_event_to_id;
use crate::session::{SessionEvent, SessionStream};
use crate::theme::Theme;
use crate::view::AgentView;
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self};
use ratatui::Terminal;
use std::io::stdout;
use std::time::Duration;

pub struct AppOptions {
    pub theme: String,
    /// Replay delay per entry while streaming history (ms). 0 = instant load.
    pub replay_delay_ms: u64,
    /// Auto-exit after this many ms of runtime (headless verification).
    pub auto_exit_ms: Option<u64>,
    /// Panic after the first paint (the exit-restore verifier's panic-path driver: a real unwind
    /// on the live surface must still leave the terminal whole).
    pub panic_after_frame: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            theme: "prime".to_string(),
            replay_delay_ms: 0,
            auto_exit_ms: None,
            panic_after_frame: false,
        }
    }
}

#[must_use]
pub fn load_theme(name: &str) -> Theme {
    let mode = crate::theme::detect_color_mode();
    // The default brand theme when the caller passes none (empty) or an
    // unknown name; only known builtins resolve.
    let known = ["prime", "dark", "light"];
    let name = if known.contains(&name) { name } else { "prime" };
    Theme::builtin(name, mode)
}

/// Run the view against a session stream until the stream ends and the user exits. `on_submit`
/// receives editor submissions (unused in replay mode).
///
/// # Errors
///
/// Returns `Err` when the surface fails to mount or the replay loop fails; the exit restore runs
/// first, so the shell never keeps a TUI-state terminal.
pub fn run_app(
    stream: Box<dyn SessionStream>,
    options: &AppOptions,
    on_submit: Box<dyn FnMut(&str) + Send>,
) -> Result<()> {
    match run_app_surface(stream, options, on_submit) {
        Ok(()) => Ok(()),
        Err(error) => {
            crate::exit_restore::restore_terminal();
            Err(error)
        }
    }
}

fn run_app_surface(
    mut stream: Box<dyn SessionStream>,
    options: &AppOptions,
    mut on_submit: Box<dyn FnMut(&str) + Send>,
) -> Result<()> {
    // The TS theme emits raw ANSI color codes regardless of NO_COLOR; match
    // that so the same terminal renders the same frames either way.
    crossterm::style::force_color_output(true);
    // A panic anywhere between the mount below and the deliberate teardown must still hand
    // the terminal back whole (the same unwind-guard contract the session surface arms).
    let _surface_restore = crate::exit_restore::SurfaceRestore::armed();
    // Enable Windows VT processing before raw ANSI mode writes.
    pa_types::platform::console_init();
    // The raw-mode bracket's `cfmakeraw` write clears IXON, the kernel's one trigger for
    // lifting a pending Ctrl+S stop (see the flow e2e's launch route).
    terminal::enable_raw_mode()?;
    // The alternate screen mounts through the ownership module (the same `pendingAltScreenHandoff`
    // semantics the session surface uses), so the alt-screen state is tracked for every exit path.
    crate::altscreen::enter()?;
    // The replay surface owns the same enhanced-key modes as the session: bracketed pastes
    // arrive as one chunk.
    crate::enhanced_keys::enable(&mut std::io::stdout())?;
    let mut terminal = Terminal::new(crate::frame_output::stdout_chat_backend())?;

    let theme = load_theme(&options.theme);
    let mut view = AgentView::new(theme);
    let mut running = true;
    let start = std::time::Instant::now();
    let mut stream_ended = false;

    loop {
        if !stream_ended {
            match stream.poll()? {
                SessionEvent::Item(item) => {
                    if let crate::session::TranscriptItem::ModelChange { provider, model_id } =
                        &item
                    {
                        view.chrome.model_id = Some(model_id.clone());
                        view.chrome.model_provider = Some(provider.clone());
                    }
                    view.push(item);
                    if options.replay_delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(options.replay_delay_ms));
                    }
                }
                SessionEvent::End => stream_ended = true,
            }
        }

        let (_w, h) = crossterm::terminal::size()?;
        view.set_terminal_rows(h);
        draw(&mut terminal, &mut view)?;
        // The verifier's panic driver: the unwind must cross the live
        // surface's unwind guard, not the already-restored exit.
        assert!(
            !options.panic_after_frame,
            "pa-tui-replay: --panic-exit reached"
        );

        let timeout = Duration::from_millis(if stream_ended { 50 } else { 5 });
        if crossterm::event::poll(timeout)? {
            match crossterm::event::read()? {
                Event::Key(key) => {
                    handle_key(&mut view, key, &mut running, &mut *on_submit);
                }
                Event::Paste(text) => {
                    view.editor.handle_paste(&text);
                }
                _ => {}
            }
        }
        // Materialize once per loop turn: a parked request resolves right after its key, and a
        // background `@` search lands on a quiet turn.
        view.editor.materialize_autocomplete();
        let _ = view.editor.take_events();

        if let Some(ms) = options.auto_exit_ms {
            if start.elapsed() >= Duration::from_millis(ms) {
                running = false;
            }
        }
        if !running && stream_ended {
            break;
        }
        if !stream_ended {
            continue;
        }
        if !running {
            break;
        }
    }

    crate::exit_restore::restore_terminal();
    Ok(())
}

fn handle_key(
    view: &mut AgentView,
    key: KeyEvent,
    running: &mut bool,
    on_submit: &mut dyn FnMut(&str),
) {
    // App-level bindings: ctrl+c exits the app shell in replay mode; escape cancels
    // autocomplete.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if view.editor.is_showing_autocomplete() {
            view.editor.cancel_autocomplete();
            return;
        }
        *running = false;
        return;
    }
    if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
        *running = false;
        return;
    }
    if key.code == KeyCode::Esc {
        view.editor.cancel_autocomplete();
        return;
    }
    if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
        // Ctrl+O cycles conversation detail: overview -> details -> all -> overview.
        view.cycle_detail();
        return;
    }
    let Some(id) = key_event_to_id(&key) else {
        return;
    };
    // Transcript viewport keys (consumed before the editor in fullscreen): page scroll,
    // top, follow.
    let (page_up, page_down, to_top, follow) = {
        let kb = view.editor.keybindings();
        (
            kb.matches(&id, "tui.viewport.pageUp"),
            kb.matches(&id, "tui.viewport.pageDown"),
            kb.matches(&id, "tui.viewport.top"),
            kb.matches(&id, "tui.viewport.follow"),
        )
    };
    if page_up {
        view.scroll_by(-(view.page_size() as isize));
        return;
    }
    if page_down {
        view.scroll_by(view.page_size() as isize);
        return;
    }
    if to_top {
        view.scroll_to_top();
        return;
    }
    if follow {
        view.scroll_to_bottom();
        return;
    }
    let is_paste_marker_key = false;
    let _ = is_paste_marker_key;
    view.editor.handle_input(&id);
    dispatch_events(&mut view.editor, on_submit);
}

pub fn dispatch_events(editor: &mut Editor, on_submit: &mut dyn FnMut(&str)) {
    for ev in editor.take_events() {
        match ev {
            EditorEvent::Submitted(text) => {
                editor.add_to_history(&text);
                on_submit(&text);
            }
            // This minimal harness owns no terminal clipboard channel; the full session UI
            // performs the copy.
            EditorEvent::Changed(_)
            | EditorEvent::AutocompleteToggled(_)
            | EditorEvent::SlashMenuAccepted
            | EditorEvent::ClipboardWrite(_) => {}
        }
    }
}

/// Paint one frame. Every byte of the frame (the mount sequences on the first draw, the
/// synchronized-output bracket, the cell diff, and the cursor update) collects in the backend's
/// [`crate::frame_output::FrameWriter`] and reaches the terminal as one write with one flush; a
/// frame whose diff is empty and whose cursor did not move writes nothing at all.
pub(crate) fn draw<W: std::io::Write>(
    terminal: &mut Terminal<
        ratatui::backend::CrosstermBackend<
            crate::hyperlinks::HyperlinkWriter<crate::frame_output::FrameWriter<W>>,
        >,
    >,
    view: &mut AgentView,
) -> Result<()> {
    use ratatui::backend::Backend;
    // The bytes an empty cell diff still writes (the backend's closing style resets): a frame
    // body equal to them changed nothing on screen.
    static EMPTY_DIFF: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let empty_diff = EMPTY_DIFF.get_or_init(|| {
        let mut probe = ratatui::backend::CrosstermBackend::new(Vec::new());
        let _ = probe.draw(std::iter::empty());
        probe.writer().clone()
    });
    // The mount sequences (the alt-screen adopt/enter, the queued clear, the cursor hide) ride
    // THIS draw's single write: the first paint is the mount, and a mid-gap flush can never
    // carry the clear out early over it.
    let mounting = crate::altscreen::take_first_draw_mount();
    terminal.backend_mut().writer_mut().inner_mut().begin();
    let show_hardware_cursor = view.show_hardware_cursor;
    let mut paint = || -> Result<bool> {
        if mounting {
            let backend = terminal.backend_mut();
            crate::altscreen::enter_queued(backend)?;
            crossterm::queue!(
                backend,
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::Hide
            )?;
            backend.writer_mut().inner_mut().cursor_visible = Some(false);
        }
        // OSC 133 describes shell prompts, not alternate-screen rows. In iTerm2 it can
        // disable soft alternate-screen mode and let cursor moves clear the painted grid.
        // Keep those markers in the main-screen exit transcript only.
        // Fullscreen paint brackets the row diff in synchronized output so terminals never
        // display an intermediate, partly scrolled frame; a terminal without mode 2026 support
        // ignores the two escape sequences.
        crossterm::queue!(terminal.backend_mut(), terminal::BeginSynchronizedUpdate)?;
        // The resize check `Terminal::draw` runs first, done here (inside the bracket, as
        // there) so the frame renders at the size the diff paints at; a resize's clear makes
        // the next diff a full repaint.
        terminal.autoresize()?;
        let body_start = terminal.backend_mut().writer_mut().inner_mut().len();
        let stamp = terminal.backend_mut().writer_mut().inner_mut().stamp();
        let frame_area = terminal.get_frame().area();
        let width = frame_area.width as usize;
        let height = frame_area.height as usize;
        let frame = view.render_frame(width, height);
        let cursor = view.frame_cursor();
        // The frame's embedded OSC 8 sequences drive the paint backend's hyperlink injection;
        // install the ranges before the paint (which strips the sequences from the painted cells).
        crate::hyperlinks::install_frame(&frame);
        let mut next = ratatui::buffer::Buffer::empty(frame_area);
        ratatui::widgets::Widget::render(
            ratatui::text::Text::from(
                frame
                    .iter()
                    .map(crate::markdown::to_ratatui_line)
                    .collect::<Vec<_>>(),
            ),
            frame_area,
            &mut next,
        );
        // A following transcript that scrolled shifts its region in the terminal (DECSTBM + SU)
        // inside this frame's bracket and write, and the diff repaints only the rows the shift
        // did not predict; a frame whose shift and diff are both empty still writes nothing.
        let mut overlay = crate::inline_image::overlay::OVERLAY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let placements = view.image_placements().to_vec();
        // A terminal-side shift would move the overlay's pictures with the text: while one is
        // (or is about to be) drawn, the frame paints as a plain cell diff.
        let region = view
            .transcript_scroll_region()
            .filter(|_| placements.is_empty() && !overlay.holds_drawings());
        crate::scroll_region::PAINTER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .draw(terminal, next, region, stamp, |terminal| {
                terminal.flush()?;
                // The pixel overlay draws over the cells the diff just painted (and takes back
                // what moved); its bytes ride the same bracket and write.
                if !placements.is_empty() || overlay.holds_drawings() {
                    let painted = terminal.current_buffer_mut().clone();
                    overlay.paint(terminal.backend_mut(), &painted, &placements)?;
                }
                let diff_painted = terminal
                    .backend_mut()
                    .writer_mut()
                    .inner_mut()
                    .since(body_start)
                    != empty_diff;
                // The hardware cursor is positioned at the focused caret for IME on every changed
                // frame, but only shown when `showHardwareCursor` is on. The visibility and the
                // position are sent only when they change (ratatui's own `draw` re-sends the hide
                // on every frame); a painted diff moved the terminal's cursor, so it re-sends the
                // caret position.
                let caret = cursor
                    .filter(|(row, col)| *row < height && *col < width)
                    .map(|(row, col)| (col as u16, row as u16));
                let visible = show_hardware_cursor && caret.is_some();
                let known_visible = terminal
                    .backend_mut()
                    .writer_mut()
                    .inner_mut()
                    .cursor_visible;
                let visibility_changed = known_visible != Some(visible);
                if visibility_changed {
                    if visible {
                        terminal.show_cursor()?;
                    } else {
                        terminal.hide_cursor()?;
                    }
                }
                let known_caret = terminal.backend_mut().writer_mut().inner_mut().cursor_at;
                let caret_moved = caret.is_some() && (diff_painted || known_caret != caret);
                if let Some((col, row)) = caret.filter(|_| caret_moved) {
                    terminal.set_cursor_position(ratatui::layout::Position::new(col, row))?;
                }
                // The hyperlink writer closes a link region still open at the frame's end.
                Backend::flush(terminal.backend_mut())?;
                let writer = terminal.backend_mut().writer_mut().inner_mut();
                writer.cursor_visible = Some(visible);
                if caret.is_some() {
                    writer.cursor_at = caret;
                } else if diff_painted {
                    writer.cursor_at = None;
                }
                // The window title follows the session state; it rides the same bracket and
                // write, and is queued only when its text changes (the spinner repaints tick it),
                // so an idle frame still writes nothing.
                let title_start = writer.len();
                crate::terminal_title::sync(writer, view)?;
                let titled = writer.len() != title_start;
                Ok(mounting || diff_painted || visibility_changed || caret_moved || titled)
            })
    };
    let painted = paint();
    let writer = terminal.backend_mut().writer_mut().inner_mut();
    if let Ok(false) = painted {
        writer.discard();
    } else {
        // Always release the terminal's pending update, including on paint errors.
        crossterm::queue!(writer, terminal::EndSynchronizedUpdate)?;
        writer.commit()?;
    }
    painted?;
    if mounting {
        // Mode setup flushes stdout, so it must follow the completed first paint.
        crate::enhanced_keys::enable(&mut stdout())?;
    }
    Ok(())
}

/// Render one frame as plain text (the headless structural dump the tmux verifier and diff
/// tests use). ANSI styling and OSC zone markers are stripped.
pub fn render_frame_text(view: &mut AgentView, width: u16, height: u16) -> Vec<String> {
    let frame = view.render_frame(width as usize, height as usize);
    frame
        .iter()
        .map(|line| {
            let mut stripped = line.clone();
            crate::osc133::strip(&mut stripped);
            crate::hyperlinks::strip_osc8(&mut stripped);
            stripped.iter().map(|s| s.content.as_str()).collect()
        })
        .collect()
}
