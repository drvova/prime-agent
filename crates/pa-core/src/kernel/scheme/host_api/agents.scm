;; Messages to, and read-only views of, the nuclear family (parent,
;; siblings, children): the `agent_message.*` and `agent_observe.*` requests.

;; Send `message` to one family member (#:to) or every one (#:all #t). #:to
;; is "parent", a sibling's or child's name or id, or a row naming one (a
;; roster row, a spawn handle, a subagent row). One member: its delivery
;; receipt. Everyone: {receipts}, one per member.
(define (agent-send message #:to [to #f] #:all [all #f])
  (cond
    [(and to all) (error "agent-send: pass #:to or #:all, not both")]
    [all
     (let ([reply (%host-call "agent_message.send" (hash 'target "all" 'message message))])
       (when (list? (hash-try-get reply 'receipts))
         (for-each (lambda (receipt) (%cell "agent-message" receipt)) (hash-ref reply 'receipts)))
       reply)]
    [(not to) (error "agent-send: pass #:to a family member, or #:all #t")]
    [else
     (let* ([role (cond
                    [(equal? to "parent") "parent"]
                    [(and (hash? to) (string? (hash-try-get to 'relationship))) (hash-ref to 'relationship)]
                    [(and (hash? to) (hash-contains? to 'rlm_child_id)) "child"]
                    [else #f])]
            [name (cond
                    [(equal? role "parent") #f]
                    [(and (hash? to) (string? (hash-try-get to 'sessionName))) (hash-ref to 'sessionName)]
                    [else (%child-selector "agent-send" to)])]
            [receipt (%host-call "agent_message.send"
                                 (%options (hash 'message message)
                                           'receiver_role role
                                           'receiver_name name))])
       ;; The receipt shows on the cell's card, under the role it was sent to.
       (%cell "agent-message" (%options receipt 'receiverRole role))
       receipt)]))

;; Wait up to #:timeout-ms (1 to 600000) for a family member's direct
;; message: {outcome, messageId, senderName, senderRole, preview,
;; blockedVia} with outcome "message", "timeout", "cycle", or "no_sender".
;; #:sender and #:sender-role narrow whose message counts.
(define (event-wait #:timeout-ms [timeout-ms #f] #:sender [sender #f] #:sender-role [sender-role #f])
  (%host-call "agent_message.wait"
              (%options (hash 'timeout_ms (%integer "event-wait" "#:timeout-ms" timeout-ms))
                        'sender sender
                        'sender_role sender-role)))

;; The family. Without a target, the whole roster, active or not:
;; {current, agents}; with #:children #t this session's direct children as
;; subagent rows instead. With a target (an active id, session id or name,
;; or id suffix), that member's row, or with #:recent #t bounded previews of
;; its recent messages: {agent, messages, limit, maxChars, truncated}, #:limit
;; 1 to 50 messages (default 8), #:max-chars 80 to 2000 characters each
;; (default 800).
(define (agent-roster [target #f]
                      #:children [children #f]
                      #:recent [recent #f]
                      #:limit [limit #f]
                      #:max-chars [max-chars #f])
  (cond
    [(and (or limit max-chars) (not recent))
     (error "agent-roster: #:limit and #:max-chars bound #:recent previews")]
    [(and children (or target recent))
     (error "agent-roster: #:children lists every child; it takes no target")]
    [(and recent (not target)) (error "agent-roster: #:recent needs a target")]
    [children (hash-ref (%host-call "rlm.list_subagents" (hash)) 'subagents)]
    [recent
     (%host-call "agent_observe.recent"
                 (hash 'target target
                       'limit (%integer "agent-roster" "#:limit" (or limit 8))
                       'max_chars (%integer "agent-roster" "#:max-chars" (or max-chars 800))))]
    [target (hash-ref (%host-call "agent_observe.get" (hash 'target target)) 'agent)]
    [else (%host-call "agent_observe.list" (hash))]))
