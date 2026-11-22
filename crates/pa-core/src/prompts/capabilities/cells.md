# cells: long cells and ending the turn

The session has one engine and it runs one cell at a time. In a session that can be woken later, a cell still running when its `timeout` (or the session's yield window) passes is not stopped: the `eval` call returns with the cell's id, the cell keeps the engine, and a `[cell-done]` notice later brings its value or error and the tail of its output. Until it ends, every other cell waits for the engine and is then refused without having run, except a cell that is exactly one of `(cell-status id)`, `(cell-wait id seconds)`, `(cell-cancel id)`, `(cell-list)`, which is answered at once. In any other session the `timeout` interrupts the cell.

- `(cell-id)`: the id of the running cell
- `(cell-status id)`: a background cell's state `{cell, status, text, output, duration-ms}`; `status` is `"running"`, `"ok"`, `"error"`, or `"interrupted"`; `text` is its value or error once it ended (`#f` before); `output` is what it printed since it went to the background
- `(cell-wait id seconds)`: wait up to `seconds` (at most 600) for a background cell to end; its state as in `cell-status`
- `(cell-cancel id)`: stop a background cell that is still running; its state
- `(cell-list)`: this session's background cells, oldest first, each `{cell, status, duration-ms}`
- `(rlm-end-turn)`: end the turn once this cell's tool batch settles: other tool calls of the same message still run, no further assistant response follows, and background work keeps running (its notices collect the results). Returns `{endedTurn #t}`; raises where the host cannot end the turn
