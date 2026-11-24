;; The daemon's event bus: event sources (a named service with a JSON schema
;; for its payloads), publications, and subscriptions that wake this session
;; with a turn when a matching event is published.

;; Define, or redefine, an event source this session owns: its record.
(define (event-source-define name schema #:description [description ""])
  (%host-call "event.define_source"
              (hash 'name name 'schema schema 'description description)))

;; Remove one of this session's sources and every subscription to it:
;; {removed, subscriptionsRemoved}.
(define (event-source-remove name)
  (%host-call "event.remove_source" (hash 'name name)))

;; Publish one event of a source this session owns: {eventId, matched}.
(define (event-publish source payload)
  (%host-call "event.publish" (hash 'source source 'payload payload)))

;; Subscribe this session to a source: the subscription. Each matching event
;; starts a turn with `prompt` and the event's payload.
(define (event-subscribe source prompt
                         #:filter [filter #f]
                         #:delivery-mode [delivery-mode #f]
                         #:label [label #f])
  (%host-call "event.subscribe"
              (%options (hash 'source source 'prompt prompt)
                        'filter filter
                        'deliveryMode delivery-mode
                        'label label)))

;; Cancel one of this session's subscriptions (its id, or the record
;; `event-subscribe` returned): the cancelled subscription.
(define (event-unsubscribe subscription)
  (%host-call "event.unsubscribe"
              (hash 'subscriptionId (if (hash? subscription)
                                        (hash-ref subscription 'id)
                                        subscription))))

;; Every source on the bus, and this session's subscriptions:
;; {sources, subscriptions}.
(define (event-list)
  (%host-call "event.list" (hash)))
