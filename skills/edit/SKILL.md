---
name: edit
description: Replace an exact, unique string in an existing file. Use for targeted single-occurrence edits to files from the Scheme kernel instead of rewriting the whole file.
---

# Edit

Make a targeted edit to an existing file by replacing one exact, unique
occurrence of a string. `old-str` must appear exactly once in the file.

Call it from the kernel:

```scheme
(edit "pkg/file.py" old new)
```

Use exact old and new strings. Inside a Scheme string write `\"` for a double
quote and `\\` for a backslash; a string may span lines. Returns
`{path, start-line, check, text}`; raises if `old-str` is missing (the error
shows where it diverges from the file) or matches more than once (widen the
snippet to make it unique).

Prefer anchored edits when you have read the file: `fs-read` tags each line
with an anchor and `fs-edit` edits by pointing at anchors, so no old text is
retyped and a stale file is rejected instead of corrupted:

```scheme
(define view (fs-read "pkg/file.py"))
(displayln (hash-ref view 'text))
(fs-edit "pkg/file.py" (hash 'replace (fs-anchor view 12) 'text "new line"))
```

`(api-help "fs")` prints the reference of both.
