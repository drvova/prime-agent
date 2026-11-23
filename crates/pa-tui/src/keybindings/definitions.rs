//! The binding table. Each definition names the surfaces that read its key
//! ([`super::KeySurface`]): a user binding takes a default key only from the actions on a surface
//! that shares keys with its own.
use super::KeySurface::{Composer, Editor, EmptyComposer, List, Panel};
use super::KeybindingDefinition;
/// TUI-level bindings (`TUI_KEYBINDINGS`).
pub const TUI_KEYBINDINGS: &[(&str, KeybindingDefinition)] = &[
    (
        "tui.editor.cursorUp",
        def!(&["up"], "Move cursor up", Editor),
    ),
    (
        "tui.editor.cursorDown",
        def!(&["down"], "Move cursor down", Editor),
    ),
    (
        "tui.editor.cursorLeft",
        def!(
            &["left", "ctrl+b"],
            "Move cursor left",
            Editor,
            Panel("model"),
            Panel("tree")
        ),
    ),
    (
        "tui.editor.cursorRight",
        def!(
            &["right", "ctrl+f"],
            "Move cursor right",
            Editor,
            Panel("model"),
            Panel("tree")
        ),
    ),
    (
        "tui.editor.cursorWordLeft",
        def!(
            &["alt+left", "ctrl+left", "alt+b"],
            "Move cursor word left",
            Editor
        ),
    ),
    (
        "tui.editor.cursorWordRight",
        def!(
            &["alt+right", "ctrl+right", "alt+f"],
            "Move cursor word right",
            Editor
        ),
    ),
    (
        "tui.editor.cursorLineStart",
        // "super+left" is the macOS Cmd+Left line-start key (see the divergence note above).
        def!(
            &["home", "ctrl+a", "super+left"],
            "Move to line start",
            Editor
        ),
    ),
    (
        "tui.editor.cursorLineEnd",
        def!(
            &["end", "ctrl+e", "super+right"],
            "Move to line end",
            Editor
        ),
    ),
    (
        "tui.editor.jumpForward",
        def!(&["ctrl+]"], "Jump forward to character", Editor),
    ),
    (
        "tui.editor.jumpBackward",
        def!(&["ctrl+alt+]"], "Jump backward to character", Editor),
    ),
    ("tui.editor.pageUp", def!(&["pageUp"], "Page up", Editor)),
    (
        "tui.editor.pageDown",
        def!(&["pageDown"], "Page down", Editor),
    ),
    (
        "tui.editor.deleteCharBackward",
        def!(
            &["backspace"],
            "Delete character backward",
            Editor,
            Panel("agents")
        ),
    ),
    (
        "tui.editor.deleteCharForward",
        def!(&["delete", "ctrl+d"], "Delete character forward", Editor),
    ),
    (
        "tui.editor.deleteWordBackward",
        def!(
            &["ctrl+w", "alt+backspace"],
            "Delete word backward",
            Editor,
            Panel("agents")
        ),
    ),
    (
        "tui.editor.deleteWordForward",
        def!(&["alt+d", "alt+delete"], "Delete word forward", Editor),
    ),
    (
        "tui.editor.deleteToLineStart",
        def!(&["ctrl+u"], "Delete to line start", Editor, Panel("agents")),
    ),
    (
        "tui.editor.deleteToLineEnd",
        def!(&["ctrl+k"], "Delete to line end", Editor),
    ),
    ("tui.editor.yank", def!(&["ctrl+y"], "Yank", Editor)),
    ("tui.editor.yankPop", def!(&["alt+y"], "Yank pop", Editor)),
    (
        "tui.editor.undo",
        def!(&["ctrl+-", "super+z"], "Undo", Editor),
    ),
    // SANCTIONED DIVERGENCE from TS (operator ask 2026-09-24, documented per the #289 precedent):
    // the ids below have no TS counterpart — the prompt bar carries the full standard text-editing
    // set instead: redo, selection (shift+arrow families, select-all), document/paragraph jumps,
    // word selection, cut/copy of the selection, and character transposition. The `super+` defaults
    // are the macOS Cmd keys (the kitty protocol delivers them as the SUPER modifier); every
    // binding stays user-configurable through keybindings.json exactly like the rest.
    (
        "tui.editor.redo",
        def!(&["ctrl+shift+z", "super+shift+z"], "Redo", Editor),
    ),
    (
        "tui.editor.cursorDocStart",
        def!(
            &["ctrl+home", "super+home", "super+up"],
            "Move to start of text",
            Editor
        ),
    ),
    (
        "tui.editor.cursorDocEnd",
        def!(
            &["ctrl+end", "super+end", "super+down"],
            "Move to end of text",
            Editor
        ),
    ),
    (
        "tui.editor.cursorParagraphUp",
        def!(&["ctrl+up"], "Move one paragraph up", Editor),
    ),
    (
        "tui.editor.cursorParagraphDown",
        def!(&["ctrl+down"], "Move one paragraph down", Editor),
    ),
    (
        "tui.editor.selectAll",
        def!(&["super+a", "ctrl+shift+a"], "Select all text", Editor),
    ),
    (
        "tui.editor.selectLeft",
        def!(&["shift+left"], "Select left by character", Editor),
    ),
    (
        "tui.editor.selectRight",
        def!(&["shift+right"], "Select right by character", Editor),
    ),
    (
        "tui.editor.selectUp",
        def!(&["shift+up"], "Select up one line", Editor),
    ),
    (
        "tui.editor.selectDown",
        def!(&["shift+down"], "Select down one line", Editor),
    ),
    (
        "tui.editor.selectWordLeft",
        def!(
            &["shift+alt+left", "shift+ctrl+left"],
            "Select left by word",
            Editor
        ),
    ),
    (
        "tui.editor.selectWordRight",
        def!(
            &["shift+alt+right", "shift+ctrl+right"],
            "Select right by word",
            Editor
        ),
    ),
    (
        "tui.editor.selectLineStart",
        def!(&["shift+home"], "Select to start of line", Editor),
    ),
    (
        "tui.editor.selectLineEnd",
        def!(&["shift+end"], "Select to end of line", Editor),
    ),
    (
        "tui.editor.selectParagraphUp",
        def!(&["shift+ctrl+up"], "Select up one paragraph", Editor),
    ),
    (
        "tui.editor.selectParagraphDown",
        // `shift+ctrl+down` is `tui.viewport.follow` (the fullscreen transcript key the
        // session dispatch consumes before the editor), so the paragraph-select default
        // is `shift+alt+down` instead.
        def!(&["shift+alt+down"], "Select down one paragraph", Editor),
    ),
    (
        "tui.editor.selectDocStart",
        def!(
            &["shift+ctrl+home", "super+shift+up"],
            "Select to start of text",
            Editor
        ),
    ),
    (
        "tui.editor.selectDocEnd",
        def!(
            &["shift+ctrl+end", "super+shift+down"],
            "Select to end of text",
            Editor
        ),
    ),
    (
        "tui.editor.transposeChars",
        def!(&["ctrl+t"], "Swap the characters around the cursor", Editor),
    ),
    (
        "tui.editor.cutSelection",
        def!(
            &["ctrl+x", "super+x"],
            "Cut the selection to the clipboard",
            Editor
        ),
    ),
    (
        "tui.editor.copySelection",
        def!(
            &["ctrl+shift+c", "super+c"],
            "Copy the selection to the clipboard",
            Editor
        ),
    ),
    (
        "tui.input.newLine",
        def!(&["shift+enter"], "Insert newline", Editor),
    ),
    ("tui.input.submit", def!(&["enter"], "Submit input", Editor)),
    (
        "tui.input.tab",
        def!(&["tab"], "Tab / autocomplete", Editor),
    ),
    (
        "tui.input.copy",
        def!(&["ctrl+c"], "Copy selection", Editor),
    ),
    (
        "tui.viewport.pageUp",
        def!(
            &["pageUp"],
            "Scroll transcript up a page (fullscreen)",
            Composer
        ),
    ),
    (
        "tui.viewport.pageDown",
        def!(
            &["pageDown"],
            "Scroll transcript down a page (fullscreen)",
            Composer
        ),
    ),
    (
        "tui.viewport.top",
        def!(
            &["shift+alt+up"],
            "Scroll transcript to top (fullscreen)",
            Composer
        ),
    ),
    (
        "tui.viewport.follow",
        def!(
            &["ctrl+shift+down"],
            "Scroll to bottom and follow output (fullscreen)",
            Composer
        ),
    ),
    (
        "tui.viewport.previousPrompt",
        // `alt+pageUp`/`alt+pageDown` sit beside the page keys and no other binding claims
        // them; `alt+up`/`alt+down` browse the queued messages and `ctrl+up`/`ctrl+down` move
        // the editor by paragraph.
        def!(
            &["alt+pageUp"],
            "Jump to the previous prompt in the transcript (fullscreen)",
            Composer
        ),
    ),
    (
        "tui.viewport.nextPrompt",
        def!(
            &["alt+pageDown"],
            "Jump to the next prompt in the transcript (fullscreen)",
            Composer
        ),
    ),
    ("tui.select.up", def!(&["up"], "Move selection up", List)),
    (
        "tui.select.down",
        def!(&["down"], "Move selection down", List),
    ),
    (
        "tui.select.pageUp",
        def!(&["pageUp"], "Selection page up", List),
    ),
    (
        "tui.select.pageDown",
        def!(&["pageDown"], "Selection page down", List),
    ),
    (
        "tui.select.top",
        def!(
            &["home", "ctrl+home", "super+home", "super+up"],
            "Selection to first item",
            List
        ),
    ),
    (
        "tui.select.bottom",
        def!(
            &["end", "ctrl+end", "super+end", "super+down"],
            "Selection to last item",
            List
        ),
    ),
    (
        "tui.select.confirm",
        def!(&["enter"], "Confirm selection", List),
    ),
    (
        "tui.select.cancel",
        def!(&["escape", "ctrl+c"], "Cancel selection", List),
    ),
];

/// App-level bindings (`KEYBINDINGS` additions in coding-agent).
pub const APP_KEYBINDINGS: &[(&str, KeybindingDefinition)] = &[
    (
        "app.interrupt",
        def!(&[], "Interrupt current operation", Composer, List),
    ),
    (
        "app.clear",
        def!(
            &["ctrl+c"],
            "Interrupt running work, else clear the draft, then exit",
            Composer,
            List
        ),
    ),
    (
        "app.input.clear",
        def!(
            &["escape"],
            "Interrupt response or clear prompt",
            Composer,
            Panel("agents")
        ),
    ),
    (
        "app.exit",
        def!(
            &["ctrl+d"],
            "Exit when editor is empty",
            EmptyComposer,
            Panel("agents")
        ),
    ),
    (
        "app.suspend",
        def!(&["ctrl+z"], "Suspend to background", Composer),
    ),
    (
        "app.model.select",
        def!(&["ctrl+l"], "Open model selector", Composer),
    ),
    (
        "app.model.toggleScope",
        def!(&["alt+s"], "Toggle model selector scope", Panel("model")),
    ),
    (
        "app.model.filterProvider",
        def!(
            &["alt+1"],
            "Model selector: cycle the provider filter",
            Panel("model")
        ),
    ),
    (
        "app.model.filterBilling",
        def!(
            &["alt+2"],
            "Model selector: cycle the billing filter",
            Panel("model")
        ),
    ),
    (
        "app.model.filterCapability",
        def!(
            &["alt+3"],
            "Model selector: cycle the capability filter",
            Panel("model")
        ),
    ),
    (
        "app.model.filterContext",
        def!(
            &["alt+4"],
            "Model selector: cycle the minimum context filter",
            Panel("model")
        ),
    ),
    (
        "app.model.sort",
        def!(&["alt+o"], "Model selector: cycle the sort", Panel("model")),
    ),
    (
        "app.model.actions",
        def!(
            &["tab"],
            "Model selector: open the highlighted model's actions",
            Panel("model")
        ),
    ),
    (
        "app.model.cycleForward",
        def!(&["alt+m"], "Cycle to the next scoped model", Composer),
    ),
    (
        "app.model.cycleBackward",
        def!(
            &["shift+alt+m"],
            "Cycle to the previous scoped model",
            Composer
        ),
    ),
    (
        "app.model.cycleRecent",
        def!(
            &["f2"],
            "Switch to the next recently used model",
            Composer,
            Panel("model")
        ),
    ),
    (
        "app.thinking.cycle",
        def!(&["shift+tab"], "Cycle thinking level", Composer),
    ),
    (
        "app.tools.expand",
        def!(&["ctrl+o"], "Cycle conversation detail", Composer),
    ),
    (
        "app.subagents.focus",
        def!(&["alt+a"], "Focus activity", Composer),
    ),
    (
        "app.context.open",
        def!(&["alt+x"], "Open the context window", Composer),
    ),
    (
        "app.changes.open",
        // `alt+g` is claimed by no other binding, editor motions included (`alt+b`/`alt+f`/
        // `alt+d`/`alt+y` are the editor's); the panel's own keys are plain letters while it
        // owns the frame.
        def!(
            &["alt+g"],
            "Open the session changes panel",
            Composer,
            Panel("changes")
        ),
    ),
    (
        "app.changes.scope",
        def!(
            &["t"],
            "Changes panel: switch between this turn and the whole session",
            Panel("changes")
        ),
    ),
    (
        "app.changes.attach",
        def!(
            &["a"],
            "Changes panel: attach the selected file's diff to the next message",
            Panel("changes")
        ),
    ),
    (
        "app.editor.external",
        def!(&["ctrl+g"], "Open external editor", Composer),
    ),
    (
        "app.prompt.stash",
        def!(&["ctrl+s"], "Stash or restore draft prompt", Composer),
    ),
    (
        "app.history.search",
        def!(&["ctrl+r"], "Search prompt history", Composer),
    ),
    (
        "app.transcript.search",
        // `/` is the search key of less, vim and transcript pagers; Alt keeps a bare `/` for
        // the composer's slash commands. No other binding claims `alt+/`, and `ctrl+/` is the
        // same byte as the undo key `ctrl+-` on a legacy terminal.
        def!(&["alt+/"], "Search the conversation (fullscreen)", Composer),
    ),
    (
        "app.transcript.searchNext",
        // The bar owns the keyboard while open: the arrows have no other use there, and
        // `ctrl+n`/`ctrl+p` are the readline next/previous keys.
        def!(
            &["down", "ctrl+n"],
            "Transcript search: next match",
            Panel("search")
        ),
    ),
    (
        "app.transcript.searchPrevious",
        def!(
            &["up", "ctrl+p"],
            "Transcript search: previous match",
            Panel("search")
        ),
    ),
    (
        "app.shortcuts.open",
        def!(
            &["?"],
            "Show the shortcut overview (on an empty prompt)",
            EmptyComposer
        ),
    ),
    (
        "app.message.followUp",
        def!(
            &["alt+enter"],
            "Queue follow-up message",
            Composer,
            Panel("agents")
        ),
    ),
    (
        "app.message.navigateOlder",
        def!(&["alt+up"], "Select older pending message", Composer),
    ),
    (
        "app.message.navigateNewer",
        def!(
            &["alt+down"],
            "Select newer pending message or draft",
            Composer
        ),
    ),
    (
        "app.message.moveEarlier",
        def!(
            &["ctrl+alt+up"],
            "Move selected pending message earlier",
            Composer
        ),
    ),
    (
        "app.message.moveLater",
        def!(
            &["ctrl+alt+down"],
            "Move selected pending message later",
            Composer
        ),
    ),
    (
        "app.clipboard.pasteImage",
        def!(&["ctrl+v"], "Paste image from clipboard", Composer),
    ),
    (
        "app.clipboard.copyLoginUrl",
        def!(&["c", "alt+c"], "Copy login URL", Panel("login")),
    ),
    (
        "app.session.new",
        def!(&[], "Start a new session", Composer),
    ),
    (
        "app.session.tree",
        def!(&[], "Open session tree", EmptyComposer),
    ),
    (
        "app.session.fork",
        def!(&[], "Fork current session", EmptyComposer),
    ),
    (
        "app.session.resume",
        def!(&[], "Resume a session", Composer),
    ),
    (
        "app.agents.back",
        def!(
            &["left"],
            "Return to parent agent scope",
            EmptyComposer,
            Panel("agents")
        ),
    ),
    (
        "app.agents.open",
        def!(&["right"], "Drill into selected agent", Panel("agents")),
    ),
    (
        "app.modal.back",
        def!(&["left"], "Go back / close the current dialog", List),
    ),
    (
        "app.modal.open",
        def!(&["right"], "Open the selected row", List),
    ),
    (
        "app.agents.reply",
        def!(&["space"], "Reply to selected agent", Panel("agents")),
    ),
    (
        "app.todos.toggle",
        def!(&["alt+t"], "Expand or collapse the todo list", Composer),
    ),
    (
        "app.question.toggle",
        def!(
            &["space"],
            "Toggle the selected choice of an agent question",
            Panel("question")
        ),
    ),
    (
        "app.agents.new",
        def!(
            &["ctrl+n"],
            "Start a new session from the agents view",
            Panel("agents")
        ),
    ),
    (
        "app.agents.delete",
        def!(
            &["ctrl+x"],
            "Stop or delete selected agent",
            Panel("agents")
        ),
    ),
    (
        "app.agents.program",
        def!(
            &["ctrl+o"],
            "Show the program that spawned subagents",
            Panel("agents")
        ),
    ),
    (
        "app.agents.rename",
        def!(
            &["ctrl+r"],
            "Rename selected agent session",
            Panel("agents")
        ),
    ),
    (
        "app.agents.folderScope",
        def!(
            &["tab"],
            "Toggle the resume list between this folder and all folders",
            Panel("agents")
        ),
    ),
    (
        "app.agents.traffic",
        def!(
            &["m"],
            "Show or hide the scoped agents view's message traffic pane",
            Panel("agents")
        ),
    ),
    (
        "app.agents.pauseMessages",
        def!(
            &["alt+p"],
            "Pause or resume agent messages to the selected agent",
            Panel("agents")
        ),
    ),
    (
        "app.agents.expand",
        def!(
            &["alt+right"],
            "Expand or collapse selected agent subagents",
            Panel("agents")
        ),
    ),
    (
        "app.tree.foldOrUp",
        def!(
            &["ctrl+left", "alt+left"],
            "Fold tree branch or move up",
            Panel("tree")
        ),
    ),
    (
        "app.tree.unfoldOrDown",
        def!(
            &["ctrl+right", "alt+right"],
            "Unfold tree branch or move down",
            Panel("tree")
        ),
    ),
    (
        "app.usage.refresh",
        def!(
            &["r"],
            "Usage page: refresh provider quotas and spend",
            Panel("usage")
        ),
    ),
    (
        "app.usage.overview",
        def!(
            &["tab"],
            "Usage page: switch between overview and details",
            Panel("usage")
        ),
    ),
    (
        "app.tree.editLabel",
        def!(&["shift+l"], "Edit tree label", Panel("tree")),
    ),
    (
        "app.tree.toggleLabelTimestamp",
        def!(&["shift+t"], "Toggle tree label timestamps", Panel("tree")),
    ),
    (
        "app.tree.filter.default",
        def!(&["ctrl+d"], "Tree filter: default view", Panel("tree")),
    ),
    (
        "app.tree.filter.noTools",
        def!(&["ctrl+t"], "Tree filter: hide tool results", Panel("tree")),
    ),
    (
        "app.tree.filter.userOnly",
        def!(
            &["ctrl+u"],
            "Tree filter: user messages only",
            Panel("tree")
        ),
    ),
    (
        "app.tree.filter.labeledOnly",
        def!(
            &["ctrl+l"],
            "Tree filter: labeled entries only",
            Panel("tree")
        ),
    ),
    (
        "app.tree.filter.all",
        def!(&["ctrl+a"], "Tree filter: show all entries", Panel("tree")),
    ),
    (
        "app.tree.filter.cycleForward",
        def!(&["ctrl+o"], "Tree filter: cycle forward", Panel("tree")),
    ),
    (
        "app.tree.filter.cycleBackward",
        def!(
            &["shift+ctrl+o"],
            "Tree filter: cycle backward",
            Panel("tree")
        ),
    ),
];
