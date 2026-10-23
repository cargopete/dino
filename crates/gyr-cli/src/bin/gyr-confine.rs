//! Confines this process, then becomes the program it was asked to run.
//!
//! Landlock cannot be applied to a child without `pre_exec`, which is `unsafe`
//! and the workspace forbids it. One level down the problem disappears: a
//! process may restrict *itself* with entirely safe calls, and Landlock
//! restrictions are inherited across `exec`. So this binary restricts itself
//! and then `exec`s, which needs no unsafe at all. RFC-0009 section 5.1.
//!
//! Usage:
//!
//! ```text
//! gyr-confine --allow-write <path>... [--allow-connect <port>]... -- <program> [args]...
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(request) = Request::parse(&arguments) else {
        eprintln!(
            "gyr-confine: usage: gyr-confine --allow-write <path>... [--allow-connect <port>]... \
             -- <program> [args]..."
        );
        return ExitCode::from(64);
    };

    match confine_and_exec(&request) {
        // `exec` only returns on failure, so reaching here at all is an error.
        Err(message) => {
            eprintln!("gyr-confine: {message}");
            ExitCode::from(126)
        }
    }
}

#[derive(Debug)]
struct Request {
    writable: Vec<String>,
    /// TCP ports a connect is allowed to. None by default.
    connect: Vec<u16>,
    program: String,
    arguments: Vec<String>,
}

impl Request {
    fn parse(arguments: &[String]) -> Option<Self> {
        let mut writable = Vec::new();
        let mut connect = Vec::new();
        let mut rest = arguments.iter();
        loop {
            match rest.next()?.as_str() {
                "--allow-write" => writable.push(rest.next()?.clone()),
                "--allow-connect" => connect.push(rest.next()?.parse().ok()?),
                "--" => break,
                _ => return None,
            }
        }
        let program = rest.next()?.clone();
        Some(Self {
            writable,
            connect,
            program,
            arguments: rest.cloned().collect(),
        })
    }
}

/// Restricts this process and replaces it with the requested program.
///
/// Returns only on failure. A confinement that could not be fully enforced is a
/// failure rather than a warning: a boundary that half applied is worse than
/// one that refused, because only one of those is visible.
#[cfg(target_os = "linux")]
fn confine_and_exec(request: &Request) -> Result<std::convert::Infallible, String> {
    use std::os::unix::process::CommandExt as _;

    use landlock::ABI;
    use landlock::Access;
    use landlock::AccessFs;
    use landlock::AccessNet;
    use landlock::CompatLevel;
    use landlock::Compatible;
    use landlock::NetPort;
    use landlock::PathBeneath;
    use landlock::PathFd;
    use landlock::Ruleset;
    use landlock::RulesetAttr;
    use landlock::RulesetCreatedAttr;
    use landlock::RulesetStatus;

    // ABI 4 is the floor RFC-0009 section 5.1 chose, because it is the first
    // that can deny the network without a second mechanism.
    let abi = ABI::V4;

    let mut ruleset = Ruleset::default()
        // Without this the crate degrades to best effort on an older kernel and
        // enforces whatever it can, silently. The whole point of a floor is that
        // falling below it is an error.
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|error| format!("cannot handle filesystem access: {error}"))?
        .handle_access(AccessNet::from_all(abi))
        .map_err(|error| format!("cannot handle network access: {error}"))?
        .create()
        .map_err(|error| format!("cannot create a Landlock ruleset: {error}"))?
        // Everything is readable, deliberately. RFC-0009 section 2 explains why
        // a narrower read profile was rejected rather than guessed at.
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|error| format!("cannot open /: {error}"))?,
            AccessFs::from_read(abi),
        ))
        .map_err(|error| format!("cannot allow reads: {error}"))?;

    for path in &request.writable {
        // A file takes only the rights a file can have; asking for directory
        // rights on one is an error.
        let access = if std::path::Path::new(path).is_file() {
            AccessFs::from_file(abi)
        } else {
            AccessFs::from_all(abi)
        };
        let handle = PathFd::new(path).map_err(|error| format!("cannot open {path}: {error}"))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(handle, access))
            .map_err(|error| format!("cannot allow writes under {path}: {error}"))?;
    }
    for port in &request.connect {
        ruleset = ruleset
            .add_rule(NetPort::new(*port, AccessNet::ConnectTcp))
            .map_err(|error| format!("cannot allow connecting to port {port}: {error}"))?;
    }

    // Bind is never allowed, and connect only to the ports named.
    let status = ruleset
        .restrict_self()
        .map_err(|error| format!("cannot restrict this process: {error}"))?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(format!(
            "Landlock was not fully enforced ({:?}); this kernel is below the ABI {} floor",
            status.ruleset, abi as i32
        ));
    }

    Err(format!(
        "cannot run {}: {}",
        request.program,
        std::process::Command::new(&request.program)
            .args(&request.arguments)
            .exec()
    ))
}

#[cfg(not(target_os = "linux"))]
fn confine_and_exec(request: &Request) -> Result<std::convert::Infallible, String> {
    let command = std::iter::once(request.program.as_str())
        .chain(request.arguments.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    Err(format!(
        "there is no Landlock on {}, so `{command}` was not run and {} write path(s) and {} \
         port(s) were not applied; gyr-confine is a Linux binary",
        std::env::consts::OS,
        request.writable.len(),
        request.connect.len()
    ))
}
