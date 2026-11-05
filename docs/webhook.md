# Reviews from GitHub: `dino webhook`

`dino webhook` listens for GitHub's `pull_request` deliveries. When a pull
request is marked ready for review, or opened as ready, it fetches the pull
request into a clone you name, checks its head out beside it, and runs a
read-only review there. The review is recorded; it is posted only if you ask.

## Setting it up

In your own config file (`~/.config/dino/config.toml`; a project file may not
set any of this):

```toml
[webhook]
repos = { "cargopete/dino" = "/Users/me/src/dino" }  # owner/name = a clone whose remote is `origin`
# secret = "..."      # or DINO_WEBHOOK_SECRET, which wins; at least 16 bytes
# model = "claude-sonnet-5.5"  # default: review_model, then model
# on_opened = true    # also review a pull request opened ready
# any_author = false  # true reviews pull requests from outside the owner, members and collaborators
# post = false        # post each review as a comment with `gh`
# max_turns = 40
# per_hour = 6        # reviews started in any hour
```

Then:

```sh
DINO_WEBHOOK_SECRET=... dino webhook          # http://127.0.0.1:7879/github
tailscale funnel 7879                         # or any proxy that ends HTTPS
```

On GitHub, add a webhook to the repository with the public URL plus `/github`,
content type `application/json`, the same secret, and the "Pull requests"
event.

## Why a separate command

`dino serve` drives one person's interactive session, behind a bearer token,
over a tunnel. A webhook has to be reachable from GitHub, which means a public
proxy in front of a port. Kept apart, opening the webhook's port to the
internet does not also open the API that drives a session, and each listener
has one way to authenticate: a token for `serve`, a signature here. The review
sessions are also not that person's session: each one is fresh, unattended and
in its own checkout.

## What a review can and cannot do

- A delivery is read only once its `X-Hub-Signature-256` HMAC verifies, compared
  in constant time. Repeats of a delivery id, and further deliveries for a head
  already in hand or reviewed, are answered and ignored; a review that fails to
  run is forgotten, so redelivering it is taken. Past `per_hour`, or with four
  reviews waiting, a delivery is refused with 429 or 503 and not remembered, so
  redelivering it later works.
- Commit names from the payload must be 40 or 64 hex digits before they reach
  git. Git runs with the clone's hooks switched off, and the diff without
  external diff drivers or text conversion.
- The session gets `read`, `search` and `list`, confined to the checkout, under
  read-only approval. No process, no `cargo`, no network, no MCP servers, no
  hooks, no finish checks.
- Nothing from the checkout goes into the system prompt: not its `AGENTS.md`,
  skills, outline, git log or `.dino/config.toml`. Only your user file is read.
  The title, description and diff go into the request, fenced, as material to
  review, with an instruction to report any request in them rather than follow
  it. That is a prompt, not a boundary: the boundary is that the session can
  only read.
- Reviews are recorded under `$XDG_STATE_HOME/dino/webhook/` (default
  `~/.local/state/dino/webhook/`), with the session log beside each. The record
  also carries the model and this session's spending. With `post = true`, only
  the review body goes to the pull request through `gh pr comment`, under a line
  saying it is automated and unchecked, with `@mentions` broken so nobody is
  notified by text the pull request put there; the model and spending are not
  posted.

Not yet run against GitHub itself; tested with signed deliveries to a local
listener and a stub model.
