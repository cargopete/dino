//! One review: a checkout of the pull request's head, a read-only session in
//! it, and a record of what it said.

use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use dino_core::Agent;
use dino_core::AgentConfig;
use dino_core::ToolError;
use dino_core::ToolFuture;
use dino_core::ToolRuntime;
use dino_core::ToolSet;
use dino_core::approval::ReadOnly;
use dino_core::prompt::PromptContext;
use dino_core::session::JsonlSessionLog;
use dino_core::session::SessionId;
use dino_core::session::SessionMeta;
use dino_protocol::ToolAction;
use dino_protocol::ToolCall;
use dino_protocol::ToolDefinition;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use super::Config;
use super::Job;
use crate::config::ApprovalMode;

/// How long fetching a pull request may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Reviews `job` and returns where the review was recorded.
///
/// # Errors
///
/// Returns an error when the pull request cannot be fetched or checked out,
/// the session cannot start, or the record cannot be written. A review that
/// ran but ended badly is recorded, with how it ended.
pub async fn review(job: &Job, config: &Config) -> Result<PathBuf> {
    let state = crate::settings::state_dir().context("no home directory to record reviews in")?;
    let record = super::record_path(&state, job);
    let checkout = record.with_extension("checkout");
    if let Some(parent) = record.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    remove_checkout(&job.clone, &checkout).await;
    let reviewed = check_out_and_review(job, config, &record, &checkout).await;
    remove_checkout(&job.clone, &checkout).await;
    let text = reviewed?;
    std::fs::write(&record, &text).with_context(|| format!("cannot write {}", record.display()))?;
    if config.post {
        post(job, &text).await?;
    }
    Ok(record)
}

async fn check_out_and_review(
    job: &Job,
    config: &Config,
    record: &Path,
    checkout: &Path,
) -> Result<String> {
    let diff = check_out(job, checkout).await?;
    let (text, model, closing) = run_session(job, config, checkout, &diff, record).await?;
    let mut recorded = format!(
        "# {}#{} at {}\n\nA read-only review on {model}, from delivery {}. {closing}.\n\n",
        job.repo,
        job.number,
        job.short_head(),
        job.delivery
    );
    recorded.push_str(&text);
    if !recorded.ends_with('\n') {
        recorded.push('\n');
    }
    Ok(recorded)
}

/// Fetches the pull request into its clone, checks its head out at
/// `checkout`, and returns its diff against the base.
async fn check_out(job: &Job, checkout: &Path) -> Result<String> {
    let pull = format!("refs/pull/{}/head", job.number);
    git(
        &job.clone,
        &["fetch", "--no-tags", "--quiet", "origin", &pull, &job.base],
        FETCH_TIMEOUT,
    )
    .await?;
    if git(
        &job.clone,
        &["cat-file", "-e", &format!("{}^{{commit}}", job.head)],
        GIT_TIMEOUT,
    )
    .await
    .is_err()
    {
        bail!(
            "{}#{} has moved on from {}; the next ready event reviews it",
            job.repo,
            job.number,
            job.short_head()
        );
    }
    let checkout_arg = checkout.to_string_lossy();
    git(
        &job.clone,
        &[
            "worktree",
            "add",
            "--detach",
            "--quiet",
            &checkout_arg,
            &job.head,
        ],
        GIT_TIMEOUT,
    )
    .await?;
    // No external diff or text conversion: .gitattributes in the pull request
    // could otherwise pick which configured driver runs.
    git(
        &job.clone,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            &format!("{}...{}", job.base, job.head),
        ],
        GIT_TIMEOUT,
    )
    .await
}

/// The session: the file tools alone, every change and process refused, and a
/// prompt that holds nothing from the checkout.
async fn run_session(
    job: &Job,
    config: &Config,
    checkout: &Path,
    diff: &str,
    record: &Path,
) -> Result<(String, String, String)> {
    let checkout = checkout
        .canonicalize()
        .with_context(|| format!("cannot find the checkout {}", checkout.display()))?;
    let session_id = SessionId::generate();
    let common = crate::CommonArgs {
        model: config.model.clone(),
        max_turns: Some(config.max_turns),
        read_only: true,
        log: Some(record.with_extension("jsonl")),
        ..crate::CommonArgs::default()
    };
    // The user file only: a project file in the checkout is the author's.
    let resolved = crate::resolve_layers(&common, crate::settings::Layers::user_only()?)?;
    let settings = crate::run_settings(&common, checkout.clone(), resolved, None, &session_id)?;

    let files = dino_tools::WorkspaceTools::new(&checkout, dino_tools::ToolLimits::default())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let tools =
        ToolSet::new(vec![Box::new(Reading(files))]).map_err(|error| anyhow::anyhow!("{error}"))?;
    let definitions = tools.definitions();
    let prompt = dino_core::prompt::system_prompt(&PromptContext {
        workspace_root: checkout.display().to_string(),
        model: Some(settings.profile.key.clone()),
        tools: definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect(),
        approval_mode: ApprovalMode::ReadOnly.label().to_owned(),
        max_model_turns: Some(settings.max_turns.get()),
        git: None,
        outline: None,
        instructions: Vec::new(),
        memory: Vec::new(),
        skills: Vec::new(),
    });
    let spending = crate::spend::Spending {
        meter: Arc::new(crate::spend::Meter::for_session(session_id.as_str())),
        cap: None,
    };
    let model = crate::metered(&settings, prompt, definitions, &spending)?;
    let log = JsonlSessionLog::create(
        &settings.log_path,
        SessionMeta {
            session_id,
            dino_version: env!("CARGO_PKG_VERSION").to_owned(),
            model_key: settings.profile.key.clone(),
            provider: settings.profile.provider,
            workspace_root: checkout.display().to_string(),
            approval_mode: ApprovalMode::ReadOnly.label().to_owned(),
            sandbox: "none needed: no tool runs a process".to_owned(),
            max_model_turns: settings.max_turns.get(),
        },
    )?;
    let mut agent = Agent::new(
        model,
        tools,
        AgentConfig {
            max_model_turns: settings.max_turns,
            ..AgentConfig::default()
        },
    )
    .with_policy(ReadOnly)
    .with_sink(log);
    let text = match agent.run(super::request(job, diff)).await {
        Ok(result) if result.text.trim().is_empty() => {
            format!(
                "The review ended ({:?}) without saying anything.",
                result.stop_reason
            )
        }
        Ok(result) => result.text,
        Err(error) => format!("The review did not finish: {error}."),
    };
    Ok((
        text,
        settings.profile.display_name.clone(),
        spending.meter.closing(),
    ))
}

const READING: [&str; 3] = ["read", "search", "list"];

/// The file tools without the two that edit, which read-only approval would
/// refuse anyway: offered, they cost turns.
struct Reading(dino_tools::WorkspaceTools);

impl ToolRuntime for Reading {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.0
            .definitions()
            .into_iter()
            .filter(|definition| READING.contains(&definition.name.as_str()))
            .collect()
    }

    fn classify(&self, call: &ToolCall) -> Result<ToolAction, ToolError> {
        if !READING.contains(&call.name.as_str()) {
            return Err(ToolError::new(format!("unknown tool {:?}", call.name)));
        }
        self.0.classify(call)
    }

    fn execute(&self, call: &ToolCall) -> ToolFuture<'_> {
        self.0.execute(call)
    }

    fn history_lost(&self) {
        self.0.history_lost();
    }
}

async fn post(job: &Job, text: &str) -> Result<()> {
    let mut body = format!(
        "**Dino review** of {}: automated and read-only. No person has checked it.\n\n",
        job.short_head()
    );
    body.push_str(&super::quiet_mentions(text));
    let mut child = Command::new("gh")
        .args([
            "pr",
            "comment",
            &job.number.to_string(),
            "--repo",
            &job.repo,
        ])
        .args(["--body-file", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("cannot run gh to post the review")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.as_bytes()).await?;
    }
    let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .context("gh took too long to post the review")??;
    if !output.status.success() {
        bail!(
            "gh could not post the review: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Runs git in `clone` with the clone's own hooks off, since what it checks out
/// is the pull request's.
async fn git(clone: &Path, arguments: &[&str], timeout: Duration) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(clone)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(arguments)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, output)
        .await
        .with_context(|| format!("git {} took too long", arguments.join(" ")))?
        .context("cannot run git")?;
    if !output.status.success() {
        let mut message = format!("git {} failed", arguments.join(" "));
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            let _ = write!(message, ": {}", stderr.trim());
        }
        bail!(message);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn remove_checkout(clone: &Path, checkout: &Path) {
    if checkout.exists() {
        let path = checkout.to_string_lossy();
        let _ = git(
            clone,
            &["worktree", "remove", "--force", &path],
            GIT_TIMEOUT,
        )
        .await;
        let _ = std::fs::remove_dir_all(checkout);
    }
    let _ = git(clone, &["worktree", "prune"], GIT_TIMEOUT).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(directory: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(arguments)
            .current_dir(directory)
            .output()
            .unwrap();
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// An origin with a base commit and a pull request's head under
    /// `refs/pull/3/head`, as GitHub keeps one, and a clone of it.
    fn repositories(root: &Path) -> (PathBuf, String, String) {
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        run(&origin, &["init", "--quiet"]);
        std::fs::write(origin.join("lib.rs"), "fn a() {}\n").unwrap();
        run(&origin, &["add", "."]);
        run(&origin, &["commit", "--quiet", "-m", "base"]);
        let base = run(&origin, &["rev-parse", "HEAD"]);
        run(&origin, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(origin.join("lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        run(&origin, &["commit", "--quiet", "-am", "head"]);
        let head = run(&origin, &["rev-parse", "HEAD"]);
        run(&origin, &["update-ref", "refs/pull/3/head", &head]);
        run(&origin, &["checkout", "--quiet", "main"]);
        run(&origin, &["branch", "--quiet", "-D", "feature"]);
        run(root, &["clone", "--quiet", "origin", "clone"]);
        (root.join("clone"), base, head)
    }

    #[test]
    fn a_review_is_offered_the_reading_tools_and_nothing_that_edits() {
        let root = std::env::temp_dir().join(format!("dino-webhook-read-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let files = dino_tools::WorkspaceTools::new(&root, dino_tools::ToolLimits::default());
        let reading = Reading(files.unwrap());
        let names: Vec<String> = reading
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(names, READING);
        let write = ToolCall {
            id: "1".to_owned(),
            name: "write_file".to_owned(),
            arguments: serde_json::json!({ "path": "a", "content": "b" }),
        };
        assert!(reading.classify(&write).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn the_head_is_checked_out_beside_the_clone_without_its_hooks() {
        let root = std::env::temp_dir().join(format!("dino-webhook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (clone, base, head) = repositories(&root);
        let hook = clone.join(".git").join("hooks").join("post-checkout");
        let marker = root.join("hook-ran");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let job = Job {
            delivery: "d".to_owned(),
            repo: "o/r".to_owned(),
            number: 3,
            title: String::new(),
            description: String::new(),
            head,
            base,
            clone: clone.clone(),
        };
        let checkout = root.join("12-head.checkout");

        let diff = check_out(&job, &checkout).await.unwrap();
        assert!(diff.contains("+fn b() {}"), "{diff}");
        assert_eq!(
            std::fs::read_to_string(checkout.join("lib.rs")).unwrap(),
            "fn a() {}\nfn b() {}\n"
        );
        assert!(!marker.exists(), "the clone's post-checkout hook ran");

        remove_checkout(&clone, &checkout).await;
        assert!(!checkout.exists());
        assert!(!run(&clone, &["worktree", "list"]).contains("12-head"));

        let moved = Job {
            head: "1".repeat(40),
            ..job
        };
        let error = check_out(&moved, &checkout).await.unwrap_err();
        assert!(error.to_string().contains("moved on"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
