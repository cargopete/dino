//! Asks a question about a repository, read-only, printing the answer as it
//! streams and each tool call as it is made.
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo run -p dino-sdk --example ask -- claude-sonnet-5.5 . "What does this crate do?"
//! ```

use std::path::PathBuf;

use dino_sdk::AgentConfig;
use dino_sdk::AgentEvent;
use dino_sdk::DinoAgent;
use dino_sdk::ModelEvent;
use dino_sdk::ReadOnly;
use dino_sdk::ToolRuntime as _;
use dino_sdk::Update;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let (Some(model), Some(workspace), Some(question)) =
        (arguments.next(), arguments.next(), arguments.next())
    else {
        return Err("usage: ask <model> <workspace> <question>".into());
    };
    let workspace = PathBuf::from(workspace).canonicalize()?;

    let tools = dino_sdk::workspace_tools(&workspace)?;
    let definitions = tools.definitions();
    let prompt = dino_sdk::system_prompt(&workspace, &model, &definitions, "read-only");
    let session = dino_sdk::connect(&model, prompt, definitions)?;
    let (sink, mut updates) = dino_sdk::updates();
    let mut agent: DinoAgent = DinoAgent::new(session, tools, AgentConfig::default())
        .with_policy(ReadOnly)
        .with_sink(sink);

    let run = agent.run(question);
    let show = async {
        while let Some(update) = updates.recv().await {
            match update {
                Update::Event(AgentEvent::Model {
                    event: ModelEvent::TextDelta { text },
                    ..
                }) => print!("{text}"),
                Update::Event(AgentEvent::ToolStarted { call, .. }) => {
                    eprintln!("\n[{} {}]", call.name, call.arguments);
                }
                Update::Finished(_) => break,
                Update::Event(_) => {}
            }
        }
    };
    let (result, ()) = tokio::join!(run, show);
    let result = result?;
    println!();
    eprintln!(
        "{:?} after {} turns",
        result.stop_reason, result.model_turns
    );
    Ok(())
}
