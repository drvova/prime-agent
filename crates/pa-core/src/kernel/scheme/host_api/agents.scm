;; Messages to, and read-only views of, the nuclear family (parent,
;; siblings, children): the `agent_message.*` and `agent_observe.*` requests.

;; Send `message` to one family member; its delivery receipt.
;; #:receiver-role is "parent", "sibling", or "child"; #:receiver-name names
;; the sibling or child (omit it for the parent).
(define (agent-message-send message #:receiver-role [receiver-role #f] #:receiver-name [receiver-name #f])
  (let ([receipt (%host-call "agent_message.send"
                             (%options (hash 'message message)
                                       'receiver_role receiver-role
                                       'receiver_name receiver-name))])
    ;; The receipt shows on the cell's card, under the role it was sent to.
    (%cell "agent-message" (%options receipt 'receiverRole receiver-role))
    receipt))

;; Send `message` to every family member: {receipts}, one per member.
(define (agent-message-broadcast message)
  (let ([reply (%host-call "agent_message.send" (hash 'target "all" 'message message))])
    (when (list? (hash-try-get reply 'receipts))
      (for-each (lambda (receipt) (%cell "agent-message" receipt)) (hash-ref reply 'receipts)))
    reply))

;; Wait up to #:timeout-ms (1 to 600000) for a family member's direct
;; message: {outcome, messageId, senderName, senderRole, preview,
;; blockedVia} with outcome "message", "timeout", "cycle", or "no_sender".
;; #:sender and #:sender-role narrow whose message counts.
(define (agent-message-wait #:timeout-ms [timeout-ms #f] #:sender [sender #f] #:sender-role [sender-role #f])
  (%host-call "agent_message.wait"
              (%options (hash 'timeout_ms (%integer "agent-message-wait" "#:timeout-ms" timeout-ms))
                        'sender sender
                        'sender_role sender-role)))

;; The whole family, active or not: {current, agents}.
(define (agent-observe-list)
  (%host-call "agent_observe.list" (hash)))

;; One family member's summary by active id, session id or name, or id
;; suffix.
(define (agent-observe-get target)
  (hash-ref (%host-call "agent_observe.get" (hash 'target target)) 'agent))

;; Bounded previews of a family member's recent messages:
;; {agent, messages, limit, maxChars, truncated}. #:limit is 1 to 50
;; messages, #:max-chars 80 to 2000 characters each.
(define (agent-observe-recent target #:limit [limit 8] #:max-chars [max-chars 800])
  (%host-call "agent_observe.recent"
              (hash 'target target
                    'limit (%integer "agent-observe-recent" "#:limit" limit)
                    'max_chars (%integer "agent-observe-recent" "#:max-chars" max-chars))))
