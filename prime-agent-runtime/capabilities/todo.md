A session todo list the user sees live; keep one for work with 3+ steps, marking items `in_progress` and `done` as you go. It persists across kernel restarts.

- `todo.set(items: list[str | dict]) -> TodoList`: create or replace (`[]` clears); dict items take `text` and optional `id`, `status` (`pending`/`in_progress`/`done`), `note`
- `todo.update(id: str, *, status=None, text=None, note=None) -> TodoList`: change one item
- `todo.get() -> TodoList`: `str(todos)` renders it; `todos.items` holds `TodoItem(id, text, status, note)`
