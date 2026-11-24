;; Web search and page reading, by the host's native engine and reader. API
;; keys stay on the host. Both calls end with their cell: an aborted or
;; timed-out cell drops the fetch.

;; `value` as a positive number of seconds, or an error naming the argument.
(define (%seconds who value)
  (if (and (real? value) (> value 0))
      value
      (error (string-append who ": #:timeout must be a positive number of seconds"))))

;; Search the web: {provider, label, answer, sources, citations,
;; relatedQuestions, failures, notes, text}. `text` is the readable numbered
;; list; each source is {title, url, snippet, publishedDate, author}; answer
;; is set by answer-synthesizing providers. site:, -site:, before:/after:,
;; intitle:, filetype:, quoted phrases, and OR in the query are honored.
;; #:provider is "auto" (keyed providers the user configured, then keyless
;; engines, falling back when one fails) or one engine id such as
;; "duckduckgo", "brave", "hackernews"; omitted, the /settings choice.
;; #:recency is "day", "week", "month", or "year"; #:timeout is seconds per
;; request. Raises when every provider fails.
(define (web-search query
                    #:provider [provider #f]
                    #:num-results [num-results #f]
                    #:recency [recency #f]
                    #:timeout [timeout #f])
  (%host-call "web.search"
              (%options (hash 'query query)
                        'provider provider
                        'numResults (and num-results (%integer "web-search" "#:num-results" num-results))
                        'recency recency
                        'timeoutSeconds (and timeout (%seconds "web-search" timeout)))))

;; Read a URL as markdown: {url, finalUrl, title, contentType, method,
;; markdown, outline, links, section, truncated, totalChars, notes, text}.
;; `source` is a URL or anything carrying one under 'url: a search source, a
;; links entry ({text, url}), an outline heading ({level, text, anchor, url}).
;; arXiv papers, GitHub files, READMEs, issues and PRs, and Stack Exchange
;; questions come from their structured sources; PDFs split into pages. A
;; long page is cut at #:max-chars (default 20000) with a note: read one span
;; with #:section (an outline anchor, a heading's text, or "L10-L40" for a
;; code view). #:timeout is the whole read's deadline in seconds (default 30).
(define (web-read source #:max-chars [max-chars #f] #:section [section #f] #:timeout [timeout #f])
  (%host-call "web.read"
              (%options (hash 'url
                              (cond
                                [(string? source) (trim source)]
                                [(and (hash? source) (string? (hash-try-get source 'url)))
                                 (trim (hash-ref source 'url))]
                                [else (error "web-read: the source must be a URL or carry one under 'url")]))
                        'maxChars (and max-chars (%integer "web-read" "#:max-chars" max-chars))
                        'section section
                        'timeoutSeconds (and timeout (%seconds "web-read" timeout)))))

;; `value` as a list of regex strings: one string, or a list of them.
(define (%patterns who name value)
  (cond
    [(string? value) (list value)]
    [(and (list? value) (equal? (filter string? value) value)) value]
    [else (error (string-append who ": " name " must be a regex string or a list of them"))]))

;; Crawl a site from `source` (a URL or anything carrying one under 'url):
;; {url, render, pages, failed, truncated, notes, text}. Each page has
;; web-read's shape; `text` is the index. Same host only (#:subdomains #t
;; adds its subdomains), robots.txt always honoured, at most #:max-pages
;; (default 20, at most 200) pages #:max-depth (default 3) links deep.
;; #:include / #:exclude are URL regexes (one or a list); #:max-chars cuts
;; each page (default 8000); #:sitemap #t also reads /sitemap.xml;
;; #:render #t loads pages in a local headless Chrome; #:timeout is the
;; whole crawl's deadline in seconds (default 60). Pages past the result's
;; output budget carry 'handle for ctx-page instead of their markdown.
(define (web-crawl source
                   #:max-pages [max-pages #f]
                   #:max-depth [max-depth #f]
                   #:include [include #f]
                   #:exclude [exclude #f]
                   #:max-chars [max-chars #f]
                   #:subdomains [subdomains #f]
                   #:sitemap [sitemap #f]
                   #:render [render #f]
                   #:timeout [timeout #f])
  (%host-call "web.crawl"
              (%options (hash 'url
                              (cond
                                [(string? source) (trim source)]
                                [(and (hash? source) (string? (hash-try-get source 'url)))
                                 (trim (hash-ref source 'url))]
                                [else (error "web-crawl: the source must be a URL or carry one under 'url")]))
                        'maxPages (and max-pages (%integer "web-crawl" "#:max-pages" max-pages))
                        'maxDepth (and max-depth (%integer "web-crawl" "#:max-depth" max-depth))
                        'include (and include (%patterns "web-crawl" "#:include" include))
                        'exclude (and exclude (%patterns "web-crawl" "#:exclude" exclude))
                        'maxChars (and max-chars (%integer "web-crawl" "#:max-chars" max-chars))
                        'subdomains subdomains
                        'sitemap sitemap
                        'render render
                        'timeoutSeconds (and timeout (%seconds "web-crawl" timeout)))))
