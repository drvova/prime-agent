;; The event bus (the daemon's, or a headless run's own): event sources (a
;; named service with a JSON schema for its payloads), publications, and
;; subscriptions that wake this session with a turn when a matching event is
;; published.

;; The scheduled jobs (`kind` "schedule", the `schedule.*` requests) or
;; heartbeats ("heartbeat", `rlm_heartbeat.*`) of this session that `name`
;; names, by id or label.
(define (%named-recurring kind name include-inactive)
  (filter (lambda (entry)
            (or (equal? (hash-try-get entry 'id) name) (equal? (hash-try-get entry 'label) name)))
          (if (equal? kind "schedule")
              (hash-ref (%host-call "schedule.list" (hash 'include_inactive include-inactive)) 'jobs)
              (hash-ref (%host-call "rlm_heartbeat.list" (hash 'include_inactive include-inactive))
                        'heartbeats))))

;; A timer hash's one key: 'at, 'every, or 'cron, or an error naming `who`.
(define (%timer-key who timer)
  (let ([keys (filter (lambda (key) (hash-contains? timer key)) (list 'at 'every 'cron))])
    (if (and (= (length keys) 1) (= (hash-length timer) 1))
        (car keys)
        (error (string-append who ": #:timer is a hash of exactly one of 'at, 'every, 'cron")))))

;; Define or redefine something that fires in this session, by `name`.
;; #:kind "source" (the default) is an event source on the bus: `schema` is
;; its payload schema, or #f with #:timer (a hash of one of 'every, 'cron,
;; 'at) or #:watch (a hash of 'path and optionally 'glob, 'recursive,
;; 'debounce-ms), which publish their own events; the record. #:kind
;; "schedule" is a scheduled job that runs #:prompt as a new turn on its
;; #:timer ('at: an ISO time, "in 30m", or a delay in seconds; 'every:
;; "30m" or seconds; 'cron: five fields, UTC), labelled `name`; one of that
;; name is cancelled first; the job. #:kind "heartbeat" is a recurring
;; prompt of this session (#:timer 'every, default every 5 minutes;
;; #:delivery-mode "steer" or "follow_up"; #:status "pause" or "resume"),
;; updated in place when one has that name; the host's reply.
(define (event-define name [schema #f]
                      #:kind [kind "source"]
                      #:description [description #f]
                      #:timer [timer #f]
                      #:watch [watch #f]
                      #:prompt [prompt #f]
                      #:delivery-mode [delivery-mode #f]
                      #:status [status #f])
  (cond
    [(equal? kind "source")
     (when (or prompt delivery-mode status)
       (error "event-define: #:prompt, #:delivery-mode and #:status are for a \"schedule\" or \"heartbeat\""))
     (%host-call "event.define_source"
                 (%options (hash 'name name 'schema schema 'description (or description ""))
                           'timer timer
                           'watch watch))]
    [(not (member kind (list "schedule" "heartbeat")))
     (error (string-append "event-define: unknown #:kind " (to-string kind)
                           "; expected \"source\", \"schedule\", or \"heartbeat\""))]
    [(or schema description watch)
     (error (string-append "event-define: a " kind " takes no schema, #:description or #:watch"))]
    [(not (string? prompt))
     (error (string-append "event-define: a " kind " needs its #:prompt"))]
    [(equal? kind "schedule")
     (when (or delivery-mode status)
       (error "event-define: #:delivery-mode and #:status are for a \"heartbeat\""))
     (unless (hash? timer)
       (error "event-define: a schedule needs its #:timer"))
     (let* ([key (%timer-key "event-define" timer)]
            [fires (hash-ref timer key)])
       (for-each (lambda (job) (%host-call "schedule.cancel" (hash 'id (hash-ref job 'id))))
                 (%named-recurring "schedule" name #f))
       (hash-ref (%host-call "schedule.add"
                             (hash 'prompt prompt
                                   'label name
                                   key (cond
                                         [(not (number? fires)) fires]
                                         [(equal? key 'cron) fires]
                                         [(equal? key 'every)
                                          (string-append (number->string (exact (round fires))) "s")]
                                         [(> fires 0) (%utc-after (exact->inexact fires))]
                                         [else (error "event-define: a numeric 'at is a delay in seconds and must be positive")])))
                 'job))]
    [else
     (let ([interval (and timer
                          (if (equal? (%timer-key "event-define" timer) 'every)
                              (hash-ref timer 'every)
                              (error "event-define: a heartbeat's #:timer is (hash 'every interval)")))]
           [named (%named-recurring "heartbeat" name #f)])
       (if (null? named)
           (let ([created (%host-call "rlm_heartbeat.create"
                                      (%options (hash 'instruction prompt 'label name)
                                                'interval interval
                                                'delivery_mode delivery-mode))])
             (if status
                 (%host-call "rlm_heartbeat.update"
                             (hash 'id (hash-ref (hash-ref created 'heartbeat) 'id) 'status status))
                 created))
           (%host-call "rlm_heartbeat.update"
                       (%options (hash 'id (hash-ref (car named) 'id) 'instruction prompt)
                                 'interval interval
                                 'status status
                                 'delivery_mode delivery-mode))))]))

;; Remove what `name` names: one of this session's sources and every
;; subscription to it ({removed, subscriptionsRemoved}), or with #:kind
;; "schedule" its scheduled job (the cancelled job) or "heartbeat" its
;; heartbeat (the host's reply), by id or label.
(define (event-remove name #:kind [kind "source"])
  (cond
    [(equal? kind "source") (%host-call "event.remove_source" (hash 'name name))]
    [(member kind (list "schedule" "heartbeat"))
     (let ([named (%named-recurring kind name #f)])
       (when (null? named)
         (error (string-append "event-remove: no " kind " of this session is named " name)))
       (if (equal? kind "schedule")
           (hash-ref (%host-call "schedule.cancel" (hash 'id (hash-ref (car named) 'id))) 'job)
           (%host-call "rlm_heartbeat.delete" (hash 'id (hash-ref (car named) 'id)))))]
    [else
     (error (string-append "event-remove: unknown #:kind " (to-string kind)
                           "; expected \"source\", \"schedule\", or \"heartbeat\""))]))

;; Publish one event of a source this session owns: {eventId, matched}.
(define (event-publish source payload)
  (%host-call "event.publish" (hash 'source source 'payload payload)))

;; Subscribe this session to a source (or, in a headless run, a topic
;; filter): the subscription. Each matching event
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

;; Every source on the bus and this session's subscriptions: {sources,
;; subscriptions}. #:kind "schedule" lists this session's scheduled jobs,
;; soonest first, and "heartbeat" its heartbeats ({heartbeats}); with
;; #:include-inactive #t finished, cancelled (and for heartbeats, all) ones
;; too.
(define (event-list #:kind [kind "source"] #:include-inactive [include-inactive #f])
  (cond
    [(equal? kind "source")
     (when include-inactive
       (error "event-list: #:include-inactive is for a \"schedule\" or \"heartbeat\" listing"))
     (%host-call "event.list" (hash))]
    [(equal? kind "schedule")
     (hash-ref (%host-call "schedule.list" (hash 'include_inactive include-inactive)) 'jobs)]
    [(equal? kind "heartbeat")
     (%host-call "rlm_heartbeat.list" (hash 'include_inactive include-inactive))]
    [else
     (error (string-append "event-list: unknown #:kind " (to-string kind)
                           "; expected \"source\", \"schedule\", or \"heartbeat\""))]))
