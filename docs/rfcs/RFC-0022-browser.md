# RFC-0022: A browser, and a crawler that does not stop

| | |
|---|---|
| Status | draft 2026-11-23, being built as `dino-browser`, `dino browse`, `dino crawl` and `extension/` |
| Date | 2026-11-23 |
| Depends on | RFC-0001, RFC-0006, RFC-0020, RFC-0021 |
| Scope | driving a real Chromium, from Dino and from Dancho; a durable crawl over it |

## 1. What it is for

`web_fetch` is one GET and html2text. It reads docs.rs well and a modern site
badly: a page built by script comes back as an empty shell, a page behind a
sign-in comes back as the sign-in page, and a listing that loads as you scroll
comes back as its first screen. Dancho meets all three daily (dancho-web #140,
the BNB download pages, shop listings).

The fix is a real browser, two of them:

- **A headless Chromium Dino starts itself**, on the ThinkPad or the Mac, for
  public pages and for crawls of thousands of them.
- **The person's own browser, through an extension**, for the pages only they
  can see, signed in as themselves. This is what Claude in Chrome does, with
  the agent being Dino rather than a hosted service.

Both are driven over the same thing, the Chrome DevTools Protocol, so every
operation above the transport (open, wait, read, click, type, scroll,
screenshot) is written once.

## 2. Shape

```text
 dino browse / dino crawl / browse tool / dancho web_fetch fallback
                          |
                   dino_browser::Page   open, settle, read, act, scroll, shot
                          |
                 dino_browser::Browser  new_page, close
                 /                      \
        Chromium (CDP websocket)     Extension bridge (websocket on 127.0.0.1)
        Target.* flat sessions        extension service worker
        headless, own profile         chrome.debugger.sendCommand per tab
```

- `Cdp` is the wire: one websocket, numbered calls, replies matched by id,
  events broadcast to whoever subscribed for that session.
- `Chromium` launches a browser binary with `--remote-debugging-port=0` and a
  throwaway profile, reads the websocket address off its stderr, and opens pages
  as flattened targets. It is supervised: a dead browser is started again and
  the page that was on it is retried, not lost.
- `Extension` listens on `127.0.0.1` and waits for the extension to dial in with
  a token. The extension opens a background tab, attaches `chrome.debugger`, and
  relays commands and events. `chrome.debugger` speaks CDP, so `Page` cannot
  tell the two apart.

## 3. Reading a page

`Page::read` injects one script (`extract.js`) and returns:

- `title`, `lang`, the final `url`, the canonical URL if the page names one;
- `markdown`: the main content, chosen by the densest block of text rather than
  the whole body, with headings, lists, tables and links kept;
- `links` with their text, `jsonld` blocks, `meta` (description, og:*);
- `actions`: the visible interactive elements, numbered, each with a role and
  a name. `click(n)` and `type(n, text)` act on those numbers, which the script
  stamps onto the elements as `data-dino-ref`, so a model never writes a
  selector.

"Settled" means the load event has fired and nothing in the DOM has changed for
500 ms, or 15 s have passed. Network-idle alone misreads pages that poll.

`Page::scroll_all` scrolls until the page stops growing, for listings.

## 4. The crawl

`dino crawl SEED... --db crawl.sqlite` is resumable by construction. All state
is in one SQLite file:

- `frontier(url PRIMARY KEY, depth, state, attempts, not_before, error)`, with
  `state` one of `queued`, `taken`, `done`, `failed`, `skipped`;
- `pages(url, final_url, status, title, lang, markdown, links, jsonld, meta,
  hash, fetched_at)`.

On start every `taken` row goes back to `queued`, so a crawl killed at any
moment, the machine included, loses at most the pages that were open. Running
the same command again carries on.

- **Scope.** Same host as the seed by default; `--prefix`, `--include REGEX`
  and `--exclude REGEX` narrow or widen it; `--depth` and `--max-pages` bound it.
- **URLs** are normalised before they reach the frontier: fragment dropped,
  `utm_*`, `fbclid` and `gclid` dropped, host lower-cased. A page whose text
  hashes the same as one already kept is recorded and not stored twice.
- **Retries.** A failed page is retried at 30 s, 2 min, 8 min, then marked
  `failed` with its error. A crashed browser counts as a failure of the page
  that was open, and is restarted.
- **Politeness.** `robots.txt` is read per host and obeyed; one request per
  host per `--delay` (default 1 s), whatever `--concurrency` is.
- `dino crawl --db crawl.sqlite --export pages.jsonl` writes what was kept.

## 5. What it will not do

"Unstoppable" here means durable: it survives crashes, restarts and flaky
networks, and finishes the frontier. It does not mean that it gets past a site
that has said no. Dino will not spoof a browser fingerprint, solve CAPTCHAs,
rotate proxies or ignore `robots.txt` in a crawl. A page that answers 403 or a
challenge is marked `failed` with the reason. This is the same line drawn in
dancho-web #140.

The extension is a different case: it is the person reading their own pages in
their own browser, one at a time, at their request. Crawls never run through it
unless `--browser extension` is given, and then still obey `robots.txt`.

## 6. Safety

- The bridge binds `127.0.0.1` only and refuses a connection without the token
  written to `~/.config/dino/browser-token` (0600). The extension is given the
  token once, in its options page.
- The extension only acts on tabs it opened itself. It never attaches to a tab
  the person already has open, so a model cannot read their mail by asking for
  "the current tab". Reading the tab the person is looking at is a separate,
  explicit button in the extension's popup ("send this tab to Dino").
- A server that browses for strangers, as Dancho does, launches with
  `Chromium::launch_guarded`: every connection goes through a loopback proxy
  (`Guard`) that resolves it, refuses it unless the caller allows every address,
  and pins it to the address checked. QUIC and unproxied WebRTC are off, so
  nothing leaves round it. A page cannot reach loopback, the LAN or a tailnet.
- In a Dino session, `browse` is approved per host exactly as `web_fetch` is
  (RFC-0021), because a page is also a way out.

## 7. Not yet

The `browse` tool inside a Dino session, and Dancho's `web_fetch` falling back
to a rendered page when the plain read is an empty shell, come after the CLI is
proven against real sites.
