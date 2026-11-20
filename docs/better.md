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
- Its reads are bounded (a small read budget and a wall-clock limit), and nothing it reads or
  writes enters the session's history. Its model calls are billed to the session.
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
