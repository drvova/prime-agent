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
