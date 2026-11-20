//! The helper's instructions: Prime Agent's own wording of the "Depth" questions from Felix
//! Rieseberg's "How to be more ambitious: A tutorial for humans with models"
//! (<https://felixrieseberg.com/how-to-be-more-ambitious-a-tutorial-for-humans-with-models/#depth>).
//!
//! The text goes out as one user message after the session's own system prompt and tools, so the
//! helper's requests reuse the session's cached prompt prefix; nothing here enters the system
//! prompt.

use std::path::Path;

use super::BetterMode;

/// What the helper may do, shared by both modes.
const READ_ONLY_RULES: &str = "\
This helper is read-only, and the host enforces it. `python_repl` here accepts only file \
reads and searches with literal arguments, one call per line: \
`print(await fs.read(\"path\", start=1, end=80))`, `print(await fs.grep(\"pattern\", \"dir\", \
glob=\"*.rs\"))`, `print(await fs.glob(\"**/*.toml\"))`, `print(await fs.find(\"Makefile\"))`. \
`bash` runs only one read-only git command (`git log`, `show`, `diff`, `status`, `ls-files`, \
`grep`, `rev-parse`). `edit` and every write are refused. Do not carry out the task.";

/// The reply contract (see [`super::reply`]).
const REPLY_FORMAT: &str = "\
Reply with exactly these blocks:
<depth>
your short answers to (1)-(4)
</depth>
<unresolved>
one reference per line that the prompt depends on and you have not located yet (leave it empty when none)
</unresolved>
<rewrite>
the rewritten prompt, ready to send as is
</rewrite>";

/// A grounding pass: the helper listed references it has not located, and gets `reads` more.
pub(super) fn grounding_pass(unresolved: &[String], reads: u32) -> String {
    let listed = unresolved.join("\n");
    format!(
        "You listed references you have not located yet:\n{listed}\n\nYou have {reads} more \
         reads. Locate them (or confirm they do not exist), then reply again with all three \
         blocks. {REPLY_FORMAT_REMINDER}"
    )
}

/// The refine pass: references the rewrite cites that do not resolve in the repository.
pub(super) fn refine_pass(unverified: &[String], reads: u32) -> String {
    let listed = unverified
        .iter()
        .map(|reference| format!("- `{reference}`"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "These references in your reply do not resolve in the repository (no such path, no file \
         defines the name, or the program is not installed):\n{listed}\n\nYou have {reads} \
         more reads. Replace each with what the repository really has, or drop it; never keep \
         a reference you have not located. Reply again with all three blocks. \
         {REPLY_FORMAT_REMINDER}"
    )
}

/// The reminder every later pass carries.
const REPLY_FORMAT_REMINDER: &str =
    "In brief mode, `<unchanged/>` alone still means the brief needs no fix.";

/// The first-pass message for one run.
pub(super) fn first_pass(mode: BetterMode, task: &str, cwd: &Path, read_budget: u32) -> String {
    let cwd = cwd.display();
    match mode {
        BetterMode::Deepen => format!(
            "<prime_better mode=\"deepen\">
Rewrite the prompt below into the version a top professional in its field would ask for: deeper, not bigger. The working directory is `{cwd}`.

{READ_ONLY_RULES} You have {read_budget} reads for this pass.

1. Find and read what the prompt touches first: the code, files, docs, tests, and logs. Ground every statement in something you read.
2. Answer briefly: (1) what a top professional would do here that the prompt does not ask for; (2) the rawest material they would start from, rather than something already summarized; (3) the specific methods and tools they would use; (4) what, other than the model or the user, would show the result is right (a test, a command, a measurement, a file to compare against).
3. Rewrite the prompt to ask for exactly that. Keep the user's goal and voice. Name the concrete files, symbols, and commands you found, each in `backticks`, and say what will prove the result right. Cite only what you located; never invent a path, name, or command.

{REPLY_FORMAT}

<prompt>
{task}
</prompt>
</prime_better>"
        ),
        BetterMode::Brief => format!(
            "<prime_better mode=\"brief\">
The brief below will be handed to a subagent that works in `{cwd}`. Check it against the repository before it goes out: every path, name, and command it mentions must exist there (or run read-only). Fix what is wrong or missing: a wrong path, a renamed symbol, a command that does not resolve, no way to tell the work is done. Keep everything else as written.

{READ_ONLY_RULES} You have {read_budget} reads for this pass.

When nothing needs fixing, reply with `<unchanged/>` alone. Otherwise answer (1)-(4) briefly in <depth> (what a top professional would add, the rawest material, the methods and tools, what would show the result is right) and return the fixed brief.

{REPLY_FORMAT}

<prompt>
{task}
</prompt>
</prime_better>"
        ),
    }
}
