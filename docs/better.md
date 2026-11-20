# `/better`: ask for the deeper version of a prompt

`/better <prompt>` rewrites a prompt into the version a top professional would ask for,
grounded in what Prime Agent reads from your own repository. The rewrite lands in the input
box above anything you typed meanwhile. It is never sent: read it, edit it, and press Enter
when it says what you want.

```
/better fix the parser
```

## Where the questions come from

The helper's questions are Felix Rieseberg's "Depth" prompt from
[How to be more ambitious: A tutorial for humans with models](https://felixrieseberg.com/how-to-be-more-ambitious-a-tutorial-for-humans-with-models/#depth).
For the prompt you gave, it asks:

1. What would a top professional do here that the prompt does not ask for?
2. What is the rawest material they would start from, instead of something already digested?
3. Which specific methods and tools would they use?
4. What, other than the model or you, would show the result is right?

Then it rewrites the prompt to ask for exactly that, naming the files, symbols, and commands
it found.

## What the helper can do

- It runs on the session's own model, provider, thinking level, and credential. Nothing
  switches implicitly.
- It only reads. Its `python_repl` accepts `fs.read`, `fs.grep`, `fs.glob`, and `fs.find`
  calls with literal arguments, and its `bash` accepts one read-only `git` command (`log`,
  `show`, `diff`, `status`, `ls-files`, `grep`, `rev-parse`) with no pipes, redirects, or
  substitutions. The host refuses every edit and write.
- Its reads adapt, with nothing to configure. A run starts with the reads this project has
  learned it needs (rewrites you discard make later runs start deeper; rewrites you send as is
  make them start lean), and gets more only while the helper still lists references it has not
  located, up to a hard ceiling and a wall-clock limit. Nothing it reads or writes enters the
  session's history. Its model calls are billed to the session.
- The result is verified mechanically. Every path, symbol, and command the rewrite cites in
  backticks must resolve in the repository (a path must exist, a symbol must occur in a file, a
  command's program must be installed; nothing is run). Unverified references go back to the
  helper for one refine pass, and any that still do not resolve are named on a last
  `Unverified (not found in the repository):` line under the rewrite.
- One run at a time. Esc cancels a running rewrite; nothing is sent either way.
- Errors are shown as they came. When the helper's reply has no rewrite, the whole reply is
  kept in the session's spill store and the error names its `out:<id>` handle and file.

## Outside the interactive UI

In print mode, RPC, and other non-interactive surfaces, `/better <prompt>` returns the
rewrite as the command's result message instead of filling an input box. Nothing is sent to
the model there either.

## From the REPL: `better(...)` and `rlm.spawn(..., better=True)`

The agent can call the same helper from its Python REPL:

```python
deeper = await better("fix the parser")
brief = await better("Add a test for parse_expr in src/parser.rs", cwd="crates/parser", mode="brief")
child = await rlm.spawn(brief_text, name="parser-tests", cwd="crates/parser", better=True)
```

`mode="brief"` checks a delegation brief against the repository from `cwd`: every path, name,
and command it mentions must exist there, or run read-only. The helper fixes what is wrong (a
wrong path, a renamed symbol, a command that does not resolve, no way to tell the work is
done) and returns the brief unchanged when nothing needs fixing. It starts from no
conversation: a brief is checked against the repository, not against the chat.
`rlm.spawn(..., better=True)` runs brief mode first and spawns the checked brief; the spawn
still waits for a session-boot slot like every other spawn, and the helper's model calls pass
the same provider admission as the session's own.

## Acceptance checks

Every rewrite and checked brief ends with an `<acceptance>` block: the checks that pass only
once the task is done, one per line.

```
<acceptance>
- run: cargo test -p parser parse_expr
- exists: tests/fixtures/expr.txt
- grep: src/parser.rs fn parse_expr
</acceptance>
```

- `run: <command>` passes when the command exits 0. It runs through the same runner as the
  autonomous quality gates: the platform shell in the session's working directory, with the
  gate timeout and output caps.
- `exists: <path>` passes when the path exists. `grep: <path> <text>` passes when the literal
  text occurs in the file, or in a file under the directory. Neither starts a process.
- A line that is none of these fails with its reason, so a malformed block never passes
  silently.

Sending a prompt that carries a block arms its checks. Each time a run ends cleanly (not on an
error or Esc), the checks run in the background. Nothing waits on them: the run settles and the
interface keeps painting while they run. Each round leaves one compact row in the transcript,
such as `Acceptance checks: 1 of 3 failed · round 1 of 3 · out:7f3a9c1e04`. The full report is
kept in the session's spill store under that `out:<id>` handle.

- When a check fails, the report goes to the model as a notice with each failure's raw exit and
  output tail. The model continues from it, and the checks run again when its turn ends.
- When every check passes, the row is only recorded and no turn runs.
- After three rounds that do not all pass, the checks stop re-running on their own, and the
  last notice asks the model to say what still fails.
- A new prompt without a block disarms the checks; a prompt with a new block replaces them.

The same checks are readable from the REPL. `better.checks()` returns where they stand
(`status` is `none`, `pending`, `passed`, or `failed`, plus the checks, the rounds run, and the
latest round's evidence). `better.evidence("out:7f3a9c1e04")` returns one round's per-check
results with each failure's raw reason. Telemetry counts the rounds and the checks that passed
and failed, and nothing else.

## Depth-aware delegation

Two kinds of depth meet when a session delegates. One is how many levels of subagents may
nest below it. The other is how deeply a brief was refined before it went out (the "Depth" in
the questions above).

- With no recursion setting (no `/rlm-max-depth`, no `rlmMaxDepth` setting, no `RLM_MAX_DEPTH`
  in the environment), the nesting bound adapts. It starts at 2 levels and stays between 1 and
  4. Two children in a row that come back from `rlm.collect` finished with their acceptance
  checks passing raise it one level. A child that fails, whose checks fail, or that a bounded
  `rlm.collect(..., timeout_ms=...)` wait times out on lowers it one level. A child inherits
  the bound as it stood when it was spawned. A setting always fixes the bound, and nothing
  adapts then.
- `rlm.spawn(..., better=True)` refines the brief as deeply as the child may still delegate.
  A child at the last level gets a lean check and is told to finish in one session. A child
  that may delegate further gets more reads and is asked for a brief it can split, each part
  with its own acceptance check.
- `rlm.collect` results carry `check_status`: `none` (the child's task carried no checks),
  `pending` (armed, no round finished yet), `passed`, or `failed`.

Telemetry counts the bound's raises and drops and the briefs scaled to it, and nothing else.
