# scheme: the kernel, its language, and this reference

The kernel is one Steel Scheme engine per session (mostly R5RS). It is not Python: there are no Python packages, no `import`, no `await`. Every call blocks its cell until it returns.

Cells and values

- A cell is the forms of one `eval` call, evaluated in order. Top-level `define`s persist for the session, across compaction too; they are lost only when the kernel restarts.
- A `define` of a name that already exists makes a new binding, and functions defined in earlier cells keep the one they were compiled against: after a later `(define (helper) ...)`, an earlier `(define (caller) (helper))` still calls the old `helper`. Define `caller` again too: resend every definition that uses the one you changed. `(set! helper ...)` changes it for earlier callers only where the compiler had not inlined it, which it does with a small function or a constant defined in the same cell as its caller, so do not rely on it. Keep what is meant to change in a box, which every reader sees: `(define handler (box f))`, `((unbox handler) x)`, `(set-box! handler g)`.
- The cell's result is the value of its last form, printed as data: a string in quotes with escapes, a hash as `#<hashmap {...}>`. Print with `(displayln x ...)` or `(display x)` to read text as it is: `(displayln (hash-ref (fs-read "a.rs") 'text))`.
- Numbers: `(/ 7 2)` is the exact `7/2`; use `quotient`, `exact->inexact`, `(exact (round x))`.
- `void` is a value, not a function: write `void`, test with `(void? x)`. A JSON null from a host reply is `void` or `#f`.

Data

- Strings: `string-append` takes strings only, so convert with `(to-string x)` or `number->string`. Also `string-join`, `split-many`, `split-whitespace`, `trim`, `substring`, `string-length`, `string-contains?`, `starts-with?`, `ends-with?`, `string-replace`, `string->number`, `string-upcase`. There is no `format`. Inside a string write `\"`, `\\`, `\n`; a literal may span lines.
- Lists: `list`, `cons`, `car`, `cdr`, `first`, `rest`, `last`, `list-ref`, `length`, `append`, `reverse`, `take`, `range`, `map`, `filter`, `foldl`, `for-each`, `apply`, `member`, `assoc`, `(sort lst <)`.
- Hashes are immutable maps, usually with symbol keys: `(hash 'a 1 'b 2)`, `(hash-ref h 'a)` (raises when the key is missing), `(hash-try-get h 'a)` (`#f` when missing), `(hash-contains? h 'a)`, `(hash-insert h 'c 3)` (a new hash), `hash-remove`, `hash-keys->list`, `hash-values->list`, `hash->list`, `hash-length`.
- JSON: `(string->jsexpr text)` (objects become hashes with symbol keys, numbers floats) and `(value->jsexpr-string value)`.
- Control: `define`, `lambda`, `let`, `let*`, named `let` for loops, `if`, `cond`, `case`, `when`, `unless`, `begin`, `set!`, `do`, `define-values`, `struct`.

Calling the API

- Optional arguments are keywords, `(rlm-spawn "task" #:name "worker")`, or for `bash` and the `fs-*` tools one options hash, `(bash "make" (hash 'timeout-ms 60000))`. A call its function does not declare is refused before anything runs, and the error says what to write: an unknown keyword or hash option (with the accepted ones and the nearest), a keyword given twice or without a value, an MCP tool argument its schema does not list, too few or too many arguments. An optional argument cannot be a symbol: pass a string.
- `bash`, `fs-*`, `edit` and `cell-*` return hashes with kebab-case keys (`'exit-code`). Functions answered by the host return the host's keys as each topic spells them (`'rlm_child_id`, `'deliveryStatus`).
- A failure raises: the cell ends with `Error: Generic: <function>: <reason>`. To continue, catch it: `(with-handler (lambda (e) (to-string e)) (fs-read "maybe.txt"))` returns the handler's value; `(error-object-message e)` is the message, and `(error "why" value ...)` raises your own. A command that exits non-zero is a result, not an error.
- Steel's own file, process, and environment functions are closed in this kernel: `open-input-file`, `open-output-file`, `read-dir`, `path-exists?`, `current-directory`, `command`, `spawn-process`, `load`, `(require "file.scm")`, `env-var` and the like raise or are undefined. Read and write files with `fs-read`, `fs-write`, `fs-edit` and `edit`, find them with `fs-glob`, `fs-find` and `fs-grep`, and run commands with `bash`: they work in the session's working directory. String ports (`open-output-string`, `with-output-to-string`) and `eval` work as usual.

- `(api-help [topic])`: print this reference. Without an argument it lists the topics; with a topic name it prints that topic, and with a function name that function's entry
- `(session-cwd)`: the session's working directory, an absolute path
- `(session-id)`: the session's id, or `#f` when the host gave none
- `(session-artifact-dir)`: the saved session's artifact directory, or `#f`
- `(host-request type payload)`: send the hash `payload` to the host as the request `type` and return its reply as a hash, or raise its error. Every function of this API is built on it or on the kernel's native tools; call it directly only for a request this reference names
- `(rlm-reload)`: re-read the skills, prompt templates, and `AGENTS.md` context files like the user's `/reload`; call it after writing or changing a skill. Returns the host's report `{skillsAdded, skillsRemoved, skills, promptTemplates, contextFiles, systemPromptChanged, diagnostics, pythonSkills}` plus `unbound`, the Python skills this kernel cannot call. After the cell, the Scheme source of new or changed skills is loaded; the system prompt's skill list follows from the next model call
- `(skills-unavailable)`: the skills whose Scheme source failed to load, each `{skill, error}`; empty when every skill loaded
