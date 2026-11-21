;; Children, sessions, and the transcript: the `rlm.*` host requests.

;; The improved `task`: the deeper prompt (#:mode "deepen"), or a delegation
;; brief checked against the repository from #:cwd (#:mode "brief").
(define (better task #:cwd [cwd #f] #:mode [mode "deepen"])
  (hash-ref (%host-call "better.run" (%options (hash 'task task 'mode mode) 'cwd cwd)) 'rewrite))

;; Where the armed acceptance checks stand:
;; {status, checks, rounds, max_rounds, running, last}.
(define (better-checks)
  (%host-call "better.checks" (hash)))

;; One check round's results by its handle ("out:<id>").
(define (better-evidence handle)
  (%host-call "better.evidence" (hash 'handle handle)))

;; Spawn a child on `prompt` and return its handle once the task is admitted:
;; {rlm_child_id, name, session_dir, model, queued_ms}. #:name is required
;; and unique among siblings; #:model is a "provider/model" selector;
;; #:better #t first checks the prompt as a brief (see `better`).
(define (rlm-spawn prompt
                   #:name [name #f]
                   #:model [model #f]
                   #:thinking [thinking #f]
                   #:cwd [cwd #f]
                   #:better [check-brief #f])
  (unless (string? name)
    (error "rlm-spawn: #:name is required and must be a string"))
  ;; The wire type stays "rlm.run": hosts of every version answer it.
  (%host-call "rlm.run"
              (hash 'prompt (if check-brief (better prompt #:cwd cwd #:mode "brief") prompt)
                    'kwargs (%options (hash 'name name) 'model model 'thinking thinking 'cwd cwd))))

;; Create and prompt a resident top-level daemon session:
;; {active_session_id, session_id, name, session_file, model}.
(define (rlm-create-session prompt
                            #:name [name #f]
                            #:model [model #f]
                            #:thinking [thinking #f]
                            #:cwd [cwd #f])
  (%host-call "rlm.create_session"
              (hash 'prompt prompt
                    'kwargs (%options (hash) 'name name 'model model 'thinking thinking 'cwd cwd))))

;; Models backed by the user's credentials that match `query`:
;; a list of {provider, id, name, selector, image_input}.
(define (rlm-find-models [query ""] #:limit [limit 8])
  (hash-ref (%host-call "rlm.find_models"
                        (hash 'query query 'limit (%integer "rlm-find-models" "#:limit" limit)))
            'models))

;; This session's direct children, one row each.
(define (rlm-list-subagents)
  (hash-ref (%host-call "rlm.list_subagents" (hash)) 'subagents))

;; Results of direct children: one target or a list of them (names, ids,
;; spawn handles, subagent rows); none selects every child. #:timeout-ms
;; waits that long for them to settle (0 is a snapshot); a timeout returns
;; the current snapshots, never an error.
(define (rlm-collect [targets '()] #:timeout-ms [timeout-ms 0])
  (hash-ref (%host-call "rlm.collect"
                        (hash 'targets (map (lambda (target) (%child-selector "rlm-collect" target))
                                            (if (list? targets) targets (list targets)))
                              'timeout_ms (%integer "rlm-collect" "#:timeout-ms" timeout-ms)))
            'results))

;; Delete one running or retained direct child; its last row.
(define (rlm-delete-subagent target)
  (hash-ref (%host-call "rlm.delete_subagent"
                        (hash 'target (%child-selector "rlm-delete-subagent" target)))
            'subagent))

;; Stop one direct child's current turn without deleting it:
;; {subagent, outcome} with outcome "aborted" or "idle".
(define (rlm-abort target)
  (%host-call "rlm.abort" (hash 'target (%child-selector "rlm-abort" target))))

;; Rename this session, or a direct child given as #:session-id (a session
;; id, a spawn handle, or a subagent row); the name the host settled on.
(define (rlm-rename new-name #:session-id [session-id #f])
  (hash-ref (%host-call "rlm.rename"
                        (%options (hash 'name new-name)
                                  'session_id (and session-id (%child-selector "rlm-rename" session-id))))
            'name))

;; Report brief progress to the parent: {accepted}, or {accepted: #f,
;; retry_after_ms} when the note was throttled.
(define (rlm-progress-note message)
  (%host-call "rlm.progress.note" (hash 'message message)))

;; End the turn once this cell's batch settles. Raises where the host cannot.
(define (rlm-end-turn)
  (%host-call "rlm.end_turn" (hash)))

;; Re-read the skills, prompt templates, and context files like /reload; the
;; host's report of what changed.
(define (rlm-reload)
  (%host-call "rlm.reload" (hash)))

;; The cell `rlm-decide` last ran in: its output port (each cell prints into
;; its own) and the id the host counts that cell's decisions under.
(define %decide-port #f)
(define %decide-cell "")

;; One bounded judgment call by the session's model: it sees `question`,
;; `options` (2 to 16 distinct strings) and #:context, and the chosen option
;; comes back verbatim. The host prints one [decide] line per decision.
(define (rlm-decide question options #:context [context #f])
  (unless (eq? %decide-port (current-output-port))
    (set! %decide-port (current-output-port))
    (set! %decide-cell (%fresh-id)))
  (let ([reply (%host-call "rlm.decide"
                           (%options (hash 'question question 'options options 'cell %decide-cell)
                                     'context context))])
    (when (string? (hash-try-get reply 'row))
      (displayln (hash-ref reply 'row)))
    (list-ref options (hash-ref reply 'index))))

;; One transcript read: this session's, a family member's (`agent`), or a
;; past session's of this project (`session`: a search hit or its id).
(define (%transcript who start limit role agent session)
  (let ([request (hash 'start (%integer who "#:start" start)
                       'limit (%integer who "#:limit" limit)
                       'roles (if (string? role) (list role) role))])
    (cond
      [(and agent session) (error (string-append who ": pass #:agent or #:session, not both"))]
      [session
       (%host-call "rlm.session.saved_messages"
                   (hash-insert request
                                'session
                                (if (hash? session) (hash-ref session 'session_id) session)))]
      [agent
       (%host-call "agent_observe.session_messages"
                   (hash-insert request 'target (%child-selector who agent)))]
      [else (%host-call "rlm.session.messages" request)])))

;; Rows #:start onward (from the end when negative), at most #:limit, of the
;; transcript messages whose role matches #:role (a glob or a list of globs):
;; a list of {index, id, message}, oldest first.
(define (rlm-session-messages #:start [start -10]
                              #:limit [limit 10]
                              #:role [role "*"]
                              #:agent [agent #f]
                              #:session [session #f])
  (hash-ref (%transcript "rlm-session-messages" start limit role agent session) 'messages))

;; How many transcript messages match #:role.
(define (rlm-session-count #:role [role "*"] #:agent [agent #f] #:session [session #f])
  (hash-ref (%transcript "rlm-session-count" 0 0 role agent session) 'total))

;; Past sessions of this project whose messages mention every word of
;; `query`: a list of {session_id, title, created_at, updated_at,
;; match_count, message_count, snippets}.
(define (rlm-session-search query #:limit [limit 10])
  (hash-ref (%host-call "rlm.session.search"
                        (hash 'query query 'limit (%integer "rlm-session-search" "#:limit" limit)))
            'hits))

;; One tool result's full text by its tool call id, from the rows the host
;; has loaded.
(define (rlm-session-tool-result tool-call-id)
  (let ([reply (%host-call "rlm.session.tool_result" (hash 'toolCallId tool-call-id))])
    (if (hash-ref reply 'found)
        (hash-ref reply 'text)
        (error (string-append "rlm-session-tool-result: no loaded tool result has the id "
                              tool-call-id
                              "; page older rows with (rlm-session-messages #:role \"toolResult\")")))))
