# Skills With Scheme Source

A skill with Scheme source is a regular markdown skill that also carries code for the agent's kernel. Prime Agent evaluates the source in the session's Scheme engine (Steel), so what it defines can be called from any `eval` cell, next to the kernel's own functions.

## Contract

- The source is `skill.scm` beside `SKILL.md`, and any `*.scm` files in a `scheme/` directory beside it. The `scheme/` files load first, in file-name order, then `skill.scm`. A skill with neither is documentation only.
- Each file is one program evaluated in the session's engine: every top-level `define` is global to the session. Name every function after the skill (`word-count-run`, `word-count-top`), so it cannot replace a kernel function or another skill's.
- Sources load when the kernel starts and again after a cell that called `(rlm-reload)`: a reload evaluates the files that are new or changed.
- A file that fails to read, parse, or evaluate makes its skill unavailable: the skill's remaining files are skipped, the other skills still load, and the failure is reported in a `[scheme-skills-unavailable]` notice and by `(skills-unavailable)`. What a file defined before it failed stays defined, and so does everything a removed skill defined, until the kernel restarts.
- Skills in the project's own directory are never loaded: only personal, package, and built-in skills run code.
- `SKILL.md` must describe each function: its call form, what it returns, and when to use it. The model learns of the functions only from there.

## Example

```
word-count/
├── SKILL.md
└── skill.scm
```

`skill.scm`:

```scheme
;; The `top` most frequent words of `text`: a list of (word . count) pairs.
(define (word-count-run text #:top [top 5])
  (let* ([words (split-whitespace text)]
         [counts (foldl (lambda (word counts)
                          (hash-insert counts word (+ 1 (or (hash-try-get counts word) 0))))
                        (hash)
                        words)]
         [ranked (sort (hash->list counts) (lambda (a b) (> (cdr a) (cdr b))))])
    (take ranked (min top (length ranked)))))
```

`SKILL.md` then documents it: "`(word-count-run text #:top)` returns the `#:top` (default 5) most frequent words as `(word . count)` pairs."

## What the source can use

- Everything a cell can: the kernel API (`bash`, `fs-read`, `web-read`, `mcp-call-tool`, `rlm-spawn`, ...; `(api-help)` prints its reference) and Steel's standard library. Optional arguments are keywords with defaults, `#:top [top 5]`.
- There are no packages to depend on and no `import`. For work that needs another language, ship a script in the skill's `scripts/` directory and run it with `bash`, giving its absolute path.
- A skill function runs inside the agent's kernel, so it can itself spawn sub-agents with `(rlm-spawn "subtask" #:name "worker")`.
- Code runs when the file loads, so keep the top level to definitions: no network, no commands, nothing slow.

## Verifying

1. Write the files, then run `(rlm-reload)`.
2. In the next cell, check `(skills-unavailable)` is empty for the skill and call its function with a small input.
3. Fix a failure in the source and run `(rlm-reload)` again; the changed file is evaluated again.
