# RFC-0020: Driving a session from another device

| | |
|---|---|
| Status | built 2026-11-02 as `dino serve`; the page has been run only against a stub, see section 8 |
| Date | 2026-11-02 |
| Depends on | RFC-0001, RFC-0006, RFC-0009, RFC-0014 |
| Scope | a headless session, a web page in front of it, and who may reach it |

## 1. What it is for

A long task on a machine at home, a ThinkPad say, and a person who is away from
it. They want to watch it, answer an approval, send a correction and stop it,
from a phone, without handing the machine to anyone else.

Claude Code does this through a relay that Anthropic runs. Dino has no service
and will not grow one, so this is the other design: the session serves its own
page, and the network that carries it is one the person already trusts,
Tailscale or an SSH tunnel.

## 2. Amending RFC-0001

RFC-0001 section 3 lists "cloud agent" and "background daemon" as non-goals.
This is neither a cloud agent nor a daemon that outlives its work: `dino serve`
is a foreground process, one session, which ends when its terminal interrupts it
(run it under `tmux` or a service manager to keep it). It is the first thing
in Dino that listens on a socket, so section 5 is the part of this RFC that
matters.

## 3. Shape

```text
 phone / laptop browser ---- Tailscale or ssh -L ----> dino serve
        page (HTML+JS)                                   |
        GET  /api/events  (server-sent events)  <--------+ Hub <-- AgentEvent sink
        POST /api/prompt                        ---------+ Hub --> session loop
        POST /api/cancel                        ---------+ Hub --> run's cancel token
        POST /api/approve                       ---------+ Hub --> approval policy
```

- `dino serve [--bind IP] [--port N]` plus every flag `dino` takes. It builds
  the same session as an interactive run, with the same tools, sandbox, policy,
  budget and log. What differs is where input comes from and where approvals are
  asked.
- The **hub** (`remote::Hub`) holds a numbered backlog of frames (the last
  4,000), the approvals waiting, and whether a run is under way. Pages subscribe
  and are sent the backlog after the frame they last saw, then live frames. A
  page that falls behind is cut off and reconnects.
- Frames are the session's `AgentEvent`s as JSON, plus `approval`, `resolved`,
  `state` and `notice`. Reasoning and argument deltas are not sent. Any string
  over 16 KiB is cut, with the amount cut said. The session log keeps all of it.
- A prompt sent while a run is under way goes to the model with its next tool
  results, exactly as a line typed at the terminal does. Sent while idle, it
  starts a run. **Slash commands are not available from a page**: `/clear`,
  `/model` and `/compact` stay at the terminal. Resuming is offered as a list
  instead, section 9.
- Stop from the page is ctrl-c: it cancels the run, and the conversation is
  intact.

## 4. Approvals

The approval policy is the one the terminal uses; only the asker changes. A
`RemoteApprover` puts the call to the pages and waits. The card shows the
tool, the resolved subject (not a description the model wrote) and the call's
arguments, up to 8 KiB. The pending approval survives a reconnect, and a run
that is cancelled while asking takes its question back.

A page is offered every answer but one (changed 2026-11-03, so that a session
can be driven wholly from a phone):

| Answer | Terminal | Page |
|---|---|---|
| allow once | yes | yes |
| allow all this turn | yes | yes |
| always for this exact tool and subject, this session | yes | yes |
| refuse | yes | yes |
| remember for later sessions | yes | no |
| always for a whole program | yes | yes |
| every command or every edit this session | yes | yes |
| run once outside the sandbox | yes | yes |

A phone can be lost, and a remembered rule is the one answer that outlasts the
session and its token. Everything else ends when `dino serve` does. The server
checks the answer against what that approval offered, so a page cannot send one
it was not given. Rules already saved for the workspace still apply, as they
would at the terminal. With `--read-only` nothing asks, and with
`--dangerously-allow-all` nothing asks either, which `dino serve` says.

A call that asks to leave the sandbox is shown with what that means (full
network, credentials, every file) and may be run once from the page.

## 5. Who may connect

The page controls a coding agent that can write files and run commands, so the
assumption is that whoever holds the means to drive it is the person.

- **Where it listens.** Loopback addresses and Tailscale addresses
  (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), and nothing else, whatever
  `--bind` says. `0.0.0.0` and LAN addresses are refused with the reason. The
  page is plain HTTP and relies on the tunnel for encryption, so an address
  the tunnel does not cover is refused rather than warned about. With no
  `--bind`, it uses `tailscale ip -4`, or falls back to loopback and prints the
  `ssh -L` line.
- **The token.** 256 bits from `/dev/urandom`, new on every start, printed once
  to the terminal inside the URL's fragment and kept nowhere else. Every
  `/api/` request needs `Authorization: Bearer <token>`, compared without
  stopping at the first differing byte. A wrong token waits 250 ms and is logged
  to the terminal with the peer's address.
- **No ambient authority.** The token is a header the page's script adds, not a
  cookie, so a request another site makes from the person's browser carries
  nothing. The fragment is never sent to the server, a log or a `Referer`; the
  script moves it to `sessionStorage` and takes it out of the address bar. It is
  refused in the query string by construction: there is no code that reads it
  there.
- **No cross-origin access.** No CORS headers are sent. The page's policy is
  `default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'`,
  with no inline script or style, no framing, `no-store` and `nosniff`.
- **No markup from the session.** The page writes model output and tool output
  with `textContent` only. A file the agent read can contain anything, and
  some of it was written by someone else.
- **Limits.** 32 connections at once, 10 s for headers and for a body, 64 KiB
  of body, 16 KiB of header buffer, prompts of 32 KiB, eight prompts waiting.
- **What it does not defend against.** Someone who can read the terminal the
  URL was printed in, or the browser's `sessionStorage`; a token that was pasted
  into a chat. There is no rate limit on tokens because there is nothing to
  guess, and no lockout because a lockout is a way for a stranger to deny the
  person their own session. A DNS rebinding page cannot act either, because it
  has no token to send. The `Host` header is therefore not checked.

## 6. Not built

- **`dino attach`**, a terminal client for a running `dino serve`. The page does
  the job over SSH from a phone; a terminal client would be a second reader of
  the same stream and can be written against it.
- **A relay**, so that a session behind NAT can be reached with no tunnel.
- **Several sessions in one page**, accounts, or per-device tokens.
- **Slash commands, attachments and images from the page.** Resume and a
  change of workspace are lists rather than commands, section 9.
- **A restart that keeps the page's place**: the backlog is in memory.
  `--resume` restores the conversation (RFC-0014), not the page.
- **TLS.** Tailscale and SSH already encrypt, and a self-signed certificate on a
  phone is a worse experience than the one it protects.
- **A budget prompt on the page.** If a spending cap is reached, its prompt is
  still the terminal's, and with no terminal it refuses.

## 7. Cost

Four dependencies that were already in the lock file through `reqwest`: `hyper`
(HTTP/1 server only), `hyper-util`, `http-body-util`, `bytes`. A server parser
written by hand would have been smaller and would have been the likeliest place
for a hole.

## 8. Verification

**Tests** (`cargo test -p dino-cli remote`): approvals answered, narrowed to the
offered set, withdrawn on cancel and re-sent to a late page; unconfined calls
offering refusal only; prompts to idle and busy sessions, and their limits;
frames cut and reasoning dropped; the page public and locked down; the token
required in the header and refused in the query, truncated, extended and
absent; bodies malformed, empty, unknown-field and oversized; wrong paths and
methods; backlog, live frames and `after=`; an approval answered over real
HTTP; the connection limit; the address rule, the Tailscale parse, token
length and entropy, and the comparison.

**Run once, 2026-11-02**, with the built binary against a stub chat endpoint on
loopback: a prompt over HTTP started a run; the `write_file` call asked, a page
answer of `remember` was refused with 400 and the call kept waiting, `once` was
accepted, the file was written and `resolved` followed; a request with no token
got 401; a prompt sent while busy was queued; stop cancelled the run and an
idle stop said so.

**Not done.** The page's script has been syntax-checked and never run in a
browser, so its layout, scrolling and the phone case are unseen. Nothing was
tried over a real Tailscale interface. The SSE stream was read with `curl` and
the test client, not with a browser's `fetch`.

## 9. Sessions, workspaces and the trajectory (2026-11-03)

Added for #241, items 6 and 7.

```text
GET  /api/sessions     the workspace's saved conversations and logs: id, /rename name, age, model
GET  /api/trace?id=    one log's trajectory, as `dino trace --json`, strings cut to 2 KiB
POST /api/resume       {"id"}   continue a listed conversation
GET  /api/workspaces   the current workspace, those in `workspaces`, and the recently used
POST /api/workspace    {"path"} end this session and start one there
```

- **A page picks; it never names.** A resume id must be a saved conversation the
  listing would return, and a trace id must match `[A-Za-z0-9_-]{1,64}` and be a
  log in the current workspace's `.dino/sessions`, so neither can leave that
  directory. A workspace must equal, byte for byte, a path the listing offers.
  There is no browsing of the filesystem from the page.
- **Where the workspaces come from.** `workspaces = [...]` in the user file,
  which a project file may not set: a repository should not be able to add a
  directory the page can reach. And the last ten workspaces an interactive or
  served session ran in, kept in `$XDG_STATE_HOME/dino/workspaces` (by default
  `~/.local/state/dino/workspaces`), outside any workspace a sandboxed process
  can write. Only directories that exist are offered.
- **A move restarts the session**, with the flags `dino serve` was started with,
  in the new workspace, which reads that workspace's own project file. The
  token, the listener and the page stay; the backlog says where it moved. If
  the new workspace will not start, the session goes back to the old one and the
  page is told why. Approval modes carry over as given: a `dino serve
  --dangerously-allow-all` that moves is allow-all there too.
- **Only while idle.** A resume or a move while a run is under way is refused
  with 409, so neither can cut a run off.
- **What a token holder gains.** The page reads the current workspace's logs
  directly, cut to 2 KiB a string, and a listed workspace's after a move. Logs
  hold tool output, so this is the session's own history, over workspaces the
  person named in advance or worked in.
