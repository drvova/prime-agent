# user: questions, the todo list, and notifications

- `(ask question [choices] #:multi #:default #:timeout)`: ask the user and wait; only for decisions the user must make. `choices` (a list of at least two strings) makes it a choice question, and the user may still type their own answer; without them the user types free text. `#:multi #t` lets the user pick any number of choices. `#:default` is the answer on a timeout or when nobody can answer; `#:timeout` is in seconds. Returns `{status, answer}`: `status` is `"answered"`, `"declined"`, `"timed_out"`, `"no_user"`, or `"cancelled"`; `answer` is a string, a list with `#:multi`, or `#f`. `"no_user"` returns at once with the default when nobody can answer (headless runs, subagents): decide reversible choices yourself and report open questions in the final answer

The session todo list is shown to the user live and persists across kernel restarts. A list is `{items}`, each item `{id, text, status, note}` with `status` `"pending"`, `"in_progress"`, or `"done"`.

- `(todo-set items)`: create or replace the list; an empty list clears it. Each item is its text, or a hash with `'text` and optional `'id`, `'status`, `'note`. Returns the whole list
- `(todo-update id #:status #:text #:note)`: change one item (`#:note ""` clears the note); the whole list
- `(todo-get)`: the current list (empty when none was set)
- `(notify message #:level)`: ping the user without waiting: they see a notice row and their terminal asks for attention. `#:level` is `"info"` (the default), `"success"`, `"warning"`, or `"error"`. Returns whether a user was attached to see it (`#f` in headless runs). Use it when long or unattended work finishes or needs a look
