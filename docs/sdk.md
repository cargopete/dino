# Driving Dino from a program

There are two entry points. From Rust, the `dino-sdk` crate. From anything
else, `dino run --json` and the JSON lines it writes.

## What is stable

What `dino-sdk` names, and the records `dino run --json` writes, change only
with a note in this file. Everything else in the workspace's crates is
internal and changes when the work needs it to: depend on `dino-core` directly
and you are tracking a moving target.

Record and event types gain fields and variants over time. A reader should
ignore fields and `type`s it does not know rather than refuse them.

## Rust: `dino-sdk`

```toml
[dependencies]
dino-sdk = { git = "https://github.com/cargopete/dino" }
tokio = { version = "1", features = ["macros", "rt"] }
```

An agent is a model session, a set of tools, a policy that decides each call,
and a sink that sees every event:

```rust
use dino_sdk::{AgentConfig, AgentEvent, DinoAgent, ModelEvent, ReadOnly, ToolRuntime as _, Update};

let tools = dino_sdk::workspace_tools(&workspace)?;
let definitions = tools.definitions();
let prompt = dino_sdk::system_prompt(&workspace, "claude-sonnet-5.5", &definitions, "read-only");
let session = dino_sdk::connect("claude-sonnet-5.5", prompt, definitions)?;
let (sink, mut updates) = dino_sdk::updates();
let mut agent = DinoAgent::new(session, tools, AgentConfig::default())
    .with_policy(ReadOnly)
    .with_sink(sink);

let run = agent.run("What does this crate do?");
let show = async {
    while let Some(update) = updates.recv().await {
        match update {
            Update::Event(AgentEvent::Model { event: ModelEvent::TextDelta { text }, .. }) => print!("{text}"),
            Update::Finished(_) => break,
            Update::Event(_) => {}
        }
    }
};
let (result, ()) = tokio::join!(run, show);
```

The whole program is `crates/dino-sdk/examples/ask.rs`, built with the rest of
the workspace:

```sh
ANTHROPIC_API_KEY=... cargo run -p dino-sdk --example ask -- claude-sonnet-5.5 . "What does this crate do?"
```

The pieces:

- `connect(model, system_prompt, tools)` opens a conversation with a model from
  `dino models`, with its credential from the environment as `dino` reads it.
  It has none of the CLI's fallback chain, effort setting or spending meter.
- `workspace_tools(root)` is `read`, `search`, `list`, `apply_patch` and
  `write_file`, confined to `root`. Processes, `cargo` and the network are not
  in it; a `ToolRuntime` of your own can add what you need, and `ToolSet::new`
  combines runtimes.
- `system_prompt(workspace, model, tools, approvals)` is Dino's prompt with the
  repository's instruction files. Any string will do instead.
- The policy decides each classified call. `ReadOnly`, the default, refuses
  every edit, process and network call; `AllowAll` refuses nothing. Implement
  `ApprovalPolicy` to ask somebody.
- `updates()` is a sink and the channel its `Update`s arrive on: every
  `AgentEvent` as it happens, then `Finished` with how the run ended. Any
  `EventSink` will do, `JsonlSessionLog` among them.
- `run` returns a `RunResult`: the events, the final text, the stop reason and
  the turns taken. `run_cancellable` takes a `CancellationToken`.

One agent holds one conversation; each `run` continues it.

## Any language: `dino run --json`

```sh
dino run --json --read-only "What does this crate do?"
```

Standard output is one JSON object a line, the same records the session log
keeps in `.dino/sessions/<id>.jsonl` (also written, as usual). Standard error
carries the human-readable status lines. The exit status is 0 when the model
finished, 130 when interrupted, and 1 otherwise.

Each record has `record`, `seq` (from 0, unbroken) and `unix_millis`:

- `{"record": "started", "session": {...}}` comes first. `session` holds
  `session_id`, `dino_version`, `model_key`, `provider`, `workspace_root`,
  `approval_mode`, `sandbox` and `max_model_turns`.
- `{"record": "event", "event": {...}}`, one per agent event. `event.type` says
  which. The ones most readers want:
  - `submitted`: `text`, the request.
  - `model`: `model_turn`, and `event`, whose `type` is `text_delta` (`text`),
    `reasoning_delta`, `tool_call_started`, `tool_call_arguments_delta`,
    `tool_call_completed` (`call`: `id`, `name`, `arguments`), `usage` (`usage`:
    token counts) or `finished` (`reason`).
  - `tool_decided`: `call_id`, `tool`, `action`, and `decision`, whose
    `outcome` is `allowed` (with its `source`) or `denied` (with a `reason`).
  - `tool_started`: `call`. `tool_finished`: `result` (`call_id`, and `output`
    with `content` and `is_error`).
  - `turn_retried` and `waiting_for_network`: a turn is being sent again; text
    streamed in the failed attempt did not happen.
- `{"record": "finished", "outcome": {...}}` comes last for each run:
  `stop_reason` (`end_turn`, `cancelled`, `blocked` and others, or null),
  `model_turns`, and `error` when the run failed.

The authoritative shapes are `AgentEvent`, `ModelEvent` and `SessionRecord` in
`crates/dino-protocol` and `crates/dino-core/src/session.rs`.

Approvals: a call the policy would ask about is asked on the terminal, which a
driving program is not. Choose a mode that does not ask: `--read-only`,
`--dangerously-allow-all` (still inside the sandbox), or `approvals` in your own
config file. When standard input is not a terminal and gives no answer, as when
it is closed, every ask is refused.

`docs/sdk_example.py` drives it from Python with the standard library alone:

```sh
python3 docs/sdk_example.py "What does this crate do?"
```
