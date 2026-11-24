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
;; #:format is 'markdown (the default), 'commonmark, 'text, 'html2text, 'html
;; or 'xml for an HTML page; #:transform is a hash of 'readability,
;; 'main-content, 'filter-images, 'filter-svg, 'clean-html (#t) and 'root,
;; 'exclude (CSS selectors) run on the page first.
(define (web-read source
                  #:max-chars [max-chars #f]
                  #:section [section #f]
                  #:timeout [timeout #f]
                  #:format [format #f]
                  #:transform [transform #f])
  (%host-call "web.read"
              (%options (hash 'url
                              (cond
                                [(string? source) (trim source)]
                                [(and (hash? source) (string? (hash-try-get source 'url)))
                                 (trim (hash-ref source 'url))]
                                [else (error "web-read: the source must be a URL or carry one under 'url")]))
                        'maxChars (and max-chars (%integer "web-read" "#:max-chars" max-chars))
                        'section section
                        'timeoutSeconds (and timeout (%seconds "web-read" timeout))
                        'format (and format (%format "web-read" format))
                        'transform (and transform (%transform "web-read" transform)))))

;; #:format as a format name string.
(define (%format who value)
  (cond
    [(symbol? value) (symbol->string value)]
    [(string? value) value]
    [else (error (string-append who ": #:format is 'markdown, 'commonmark, 'text, 'html2text, 'html or 'xml"))]))

;; #:transform as a hash of options.
(define (%transform who value)
  (if (hash? value)
      value
      (error (string-append who ": #:transform is a hash such as (hash 'main-content #t 'root \"article\")"))))

;; `value` as a list of regex strings: one string, or a list of them.
(define (%patterns who name value)
  (cond
    [(string? value) (list value)]
    [(and (list? value) (equal? (filter string? value) value)) value]
    [else (error (string-append who ": " name " must be a regex string or a list of them"))]))

;; Crawl a site from `source` (a URL or anything carrying one under 'url):
;; {url, render, pages, failed, truncated, notes, text}. Each page has
;; web-read's shape; `text` is the index. Same host only (#:subdomains #t
;; adds its subdomains), robots.txt honoured and an honest user agent
;; unless the user's `web.crawl` settings say otherwise, at most #:max-pages
;; (default 20, at most 200) pages #:max-depth (default 3) links deep.
;; #:include / #:exclude are URL regexes (one or a list); #:max-chars cuts
;; each page (default 8000); #:sitemap #t also reads /sitemap.xml;
;; #:render #t loads pages in a local headless Chrome (#:mode 'render);
;; #:mode 'smart reads over HTTP and loads in the browser only a page that
;; needs JavaScript; #:timeout is the
;; whole crawl's deadline in seconds (default 60). Pages past the result's
;; output budget carry 'handle for ctx-page instead of their markdown.
;; Reach: #:budget (a hash of path prefix to pages, "*" for all),
;; #:external-domains (hosts crawled as part of the site), #:full-resources
;; #t (PDFs, JSON and text the pages link, not only HTML), #:normalize #t,
;; #:retry (0-3), #:throttle 'auto (pace each host by its latency), #:warc
;; "path" (append every response to a WARC file). With #:render only: #:wait-idle / #:wait-delay (seconds),
;; #:wait-selector / #:wait-dom (CSS), #:viewport (width height),
;; #:javascript #f, #:actions (a hash of URL or path to automation steps),
;; #:scripts (a hash of URL or path to JavaScript to evaluate), #:block (kinds
;; 'images 'stylesheets 'scripts 'analytics 'ads and URL patterns the browser
;; refuses to load), #:init-script (JavaScript run in every document first),
;; #:locale ("de-DE"), #:timezone ("Europe/Berlin").
(define (web-crawl source
                   #:max-pages [max-pages #f]
                   #:max-depth [max-depth #f]
                   #:include [include #f]
                   #:exclude [exclude #f]
                   #:max-chars [max-chars #f]
                   #:subdomains [subdomains #f]
                   #:sitemap [sitemap #f]
                   #:render [render #f]
                   #:mode [mode #f]
                   #:timeout [timeout #f]
                   #:budget [budget #f]
                   #:external-domains [external-domains #f]
                   #:full-resources [full-resources #f]
                   #:normalize [normalize #f]
                   #:retry [retry #f]
                   #:throttle [throttle #f]
                   #:warc [warc #f]
                   #:wait-idle [wait-idle #f]
                   #:wait-delay [wait-delay #f]
                   #:wait-selector [wait-selector #f]
                   #:wait-dom [wait-dom #f]
                   #:viewport [viewport #f]
                   #:javascript [javascript #t]
                   #:actions [actions #f]
                   #:scripts [scripts #f]
                   #:block [block #f]
                   #:init-script [init-script #f]
                   #:locale [locale #f]
                   #:timezone [timezone #f]
                   #:format [format #f]
                   #:transform [transform #f]
                   #:extract [extract #f])
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
                        'mode (cond
                                [(not mode) #f]
                                [(symbol? mode) (symbol->string mode)]
                                [(string? mode) mode]
                                [else (error "web-crawl: #:mode is 'http, 'render or 'smart")])
                        'timeoutSeconds (and timeout (%seconds "web-crawl" timeout))
                        'budget budget
                        'externalDomains (and external-domains
                                              (if (string? external-domains)
                                                  (list external-domains)
                                                  external-domains))
                        'fullResources full-resources
                        'normalize normalize
                        'retry (and retry (%integer "web-crawl" "#:retry" retry))
                        'throttle (cond
                                    [(not throttle) #f]
                                    [(symbol? throttle) (symbol->string throttle)]
                                    [(string? throttle) throttle]
                                    [else (error "web-crawl: #:throttle is 'fixed or 'auto")])
                        'warc warc
                        'waitIdleSeconds wait-idle
                        'waitDelaySeconds wait-delay
                        'waitSelector wait-selector
                        'waitDom wait-dom
                        'viewport viewport
                        'noJavascript (not javascript)
                        'actions actions
                        'scripts scripts
                        'block (and block (%block "web-crawl" block))
                        'initScript init-script
                        'locale locale
                        'timezone timezone
                        'format (and format (%format "web-crawl" format))
                        'transform (and transform (%transform "web-crawl" transform))
                        'extract (and extract (%selectors "web-crawl" "#:extract" extract)))))

;; A selectors map: names to a CSS selector or XPath expression, or a list of them.
(define (%selectors who name value)
  (if (hash? value)
      value
      (error (string-append who ": " name " is a hash of names to CSS selectors or XPath expressions"))))

;; The values `selectors` (a hash of names to a CSS selector or an XPath
;; expression, or a list of them) find on a page: a hash of each name to the
;; list of what its selectors found. `source` is a URL, anything carrying one
;; under 'url, or HTML text (starting with "<"). A URL is read as a one-page
;; crawl (robots.txt and the user's web.crawl identity apply); #:render #t or
;; #:mode 'render / 'smart loads it in the browser; #:timeout in seconds.
(define (web-extract source selectors
                     #:render [render #f]
                     #:mode [mode #f]
                     #:timeout [timeout #f])
  (let ([text (cond
                [(string? source) (trim source)]
                [(and (hash? source) (string? (hash-try-get source 'url))) (trim (hash-ref source 'url))]
                [else (error "web-extract: the source must be a URL, carry one under 'url, or be HTML")])])
    (%host-call "web.extract"
                (%options (if (and (> (string-length text) 0) (equal? (string-ref text 0) #\<))
                              (hash 'html text)
                              (hash 'url text))
                          'selectors (%selectors "web-extract" "the selectors" selectors)
                          'render render
                          'mode (cond
                                  [(not mode) #f]
                                  [(symbol? mode) (symbol->string mode)]
                                  [(string? mode) mode]
                                  [else (error "web-extract: #:mode is 'http, 'render or 'smart")])
                          'timeoutSeconds (and timeout (%seconds "web-extract" timeout))))))

;; #:block as a list of strings: a kind symbol or a URL pattern, or a list
;; of them.
(define (%block who value)
  (let ([entry (lambda (one)
                 (cond
                   [(symbol? one) (symbol->string one)]
                   [(string? one) one]
                   [else (error (string-append who ": #:block takes kind symbols and URL pattern strings"))]))])
    (if (list? value) (map entry value) (list (entry value)))))

;; `source` as a trimmed URL: a string, or anything carrying one under 'url.
(define (%capture-url who source)
  (cond
    [(string? source) (trim source)]
    [(and (hash? source) (string? (hash-try-get source 'url))) (trim (hash-ref source 'url))]
    [else (error (string-append who ": the source must be a URL or carry one under 'url"))]))

;; Load `source` in a local headless Chrome and save a PNG of it at `path`
;; (relative: under the session's working directory): {url, finalUrl, title,
;; path, bytes}. #:full-page #t captures the whole page, not the viewport;
;; #:viewport (width height); #:wait (seconds after the load); #:init-script;
;; #:timeout (seconds, default 30). The user's web.crawl identity applies.
(define (web-screenshot source path
                        #:full-page [full-page #f]
                        #:viewport [viewport #f]
                        #:wait [wait #f]
                        #:init-script [init-script #f]
                        #:timeout [timeout #f])
  (%host-call "web.capture"
              (%options (hash 'url (%capture-url "web-screenshot" source) 'path path 'kind "png")
                        'fullPage full-page
                        'viewport viewport
                        'waitSeconds wait
                        'initScript init-script
                        'timeoutSeconds (and timeout (%seconds "web-screenshot" timeout)))))

;; Load `source` in a local headless Chrome and print it to a PDF at `path`:
;; {url, finalUrl, title, path, bytes}. #:landscape #t; #:background #t
;; prints background colours and images; #:viewport, #:wait, #:init-script
;; and #:timeout as for web-screenshot.
(define (web-pdf source path
                 #:landscape [landscape #f]
                 #:background [background #f]
                 #:viewport [viewport #f]
                 #:wait [wait #f]
                 #:init-script [init-script #f]
                 #:timeout [timeout #f])
  (%host-call "web.capture"
              (%options (hash 'url (%capture-url "web-pdf" source) 'path path 'kind "pdf")
                        'landscape landscape
                        'background background
                        'viewport viewport
                        'waitSeconds wait
                        'initScript init-script
                        'timeoutSeconds (and timeout (%seconds "web-pdf" timeout)))))
