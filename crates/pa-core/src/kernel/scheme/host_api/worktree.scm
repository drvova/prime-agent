;; Isolated git worktrees for subagents: give a child its own checkout so its
;; edits stay out of yours, (rlm-spawn task #:name "lane-a" #:cwd (hash-ref wt
;; 'path)). Worktrees live under the agent dir, never inside the project, and
;; belong to this session; remove each one when its work is merged. A
;; worktree is {name, path, repo, base, exists}; wherever a function takes
;; one, its name works too. Every call ends with its cell.

;; The reply of a function the kernel runs itself: its value, or its error
;; raised.
(define (%kernel-reply reply-json)
  (let ([reply (string->jsexpr reply-json)])
    (if (hash-contains? reply 'error)
        (error (hash-ref reply 'error))
        (%exact-integers (hash-ref reply 'value)))))

(define (%worktree operation payload)
  (%kernel-reply (%worktree-call operation (value->jsexpr-string payload))))

;; A new checkout of #:base (a commit, default "HEAD") named `name` (1 to 64
;; letters, digits, ".", "_" or "-"). Unless #:carry-changes is #f, your
;; uncommitted changes (staged, unstaged, untracked) are copied into it.
(define (worktree-create name #:base [base "HEAD"] #:carry-changes [carry-changes #t])
  (%worktree "worktree-create"
             (hash 'name name 'base base 'carry_changes (if carry-changes #t #f))))

;; This session's worktrees of the current repository, by name.
(define (worktree-list)
  (%worktree "worktree-list" (hash)))

;; The worktree's changes since it was made (the carried changes excluded),
;; as a patch string.
(define (worktree-diff target)
  (%worktree "worktree-diff" (hash 'target target)))

;; Bring the worktree's changes home: {status, files, conflicts, branch, ok}
;; (branch is #f unless the changes went to one).
;; #:strategy "patch" (the default) applies them to your checkout only when
;; a `git apply` check passes, as is or three-way; on a conflict nothing is
;; applied and conflicts lists the paths (resolve them yourself, never by
;; retrying).
;; "branch" commits them to prime-agent/worktree/<name> instead. #:dry-run #t
;; only checks. status is "applied", "clean" (a dry run that would apply),
;; "conflicts", "empty" (nothing changed), or "branch".
(define (worktree-merge target #:strategy [strategy "patch"] #:dry-run [dry-run #f])
  (%worktree "worktree-merge"
             (hash 'target target 'strategy strategy 'dry_run (if dry-run #t #f))))

;; Delete the worktree; #t. Changes that were not merged refuse the removal
;; unless #:force #t.
(define (worktree-remove target #:force [force #f])
  (%worktree "worktree-remove" (hash 'target target 'force (if force #t #f))))
