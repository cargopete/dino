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
//! gyr-confine --allow-write <path>... [--allow-connect <port>]... [--deny-read <path>]...
//!     -- <program> [args]...
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
    /// Paths no read is granted beneath, such as credential directories.
    deny_read: Vec<String>,
    program: String,
    arguments: Vec<String>,
}

impl Request {
    fn parse(arguments: &[String]) -> Option<Self> {
        let mut writable = Vec::new();
        let mut connect = Vec::new();
        let mut deny_read = Vec::new();
        let mut rest = arguments.iter();
        loop {
            match rest.next()?.as_str() {
                "--allow-write" => writable.push(rest.next()?.clone()),
                "--allow-connect" => connect.push(rest.next()?.parse().ok()?),
                "--deny-read" => deny_read.push(rest.next()?.clone()),
                "--" => break,
                _ => return None,
            }
        }
        let program = rest.next()?.clone();
        Some(Self {
            writable,
            connect,
            deny_read,
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
        .map_err(|error| format!("cannot create a Landlock ruleset: {error}"))?;

    // Everything is readable except beneath the denied paths. Landlock only
    // grants, so the hole is made by granting around it. RFC-0009 section 2.
    for path in read_grants(std::path::Path::new("/"), &request.deny_read) {
        let access = if path.is_dir() {
            AccessFs::from_read(abi)
        } else {
            AccessFs::from_read(abi) & AccessFs::from_file(abi)
        };
        let Ok(handle) = PathFd::new(&path) else {
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(handle, access))
            .map_err(|error| format!("cannot allow reads under {}: {error}", path.display()))?;
    }

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

/// What to grant read access to so that everything beneath `root` is readable
/// except what lies beneath `denied`: `root` itself when nothing denied is
/// inside it, and otherwise each entry of it, recursively. Paths are compared
/// resolved, so a symlink cannot reach a denied path by another name. Entries
/// created after this runs inside a directory on a denied path's chain are not
/// readable; that is the price of a grant-only mechanism.
#[cfg(any(target_os = "linux", test))]
fn read_grants(root: &std::path::Path, denied: &[String]) -> Vec<std::path::PathBuf> {
    let denied: Vec<std::path::PathBuf> = denied
        .iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
    let mut grants = Vec::new();
    let mut visited = std::collections::HashSet::new();
    expand(root, &denied, &mut grants, &mut visited);
    grants
}

#[cfg(any(target_os = "linux", test))]
fn expand(
    path: &std::path::Path,
    denied: &[std::path::PathBuf],
    grants: &mut Vec<std::path::PathBuf>,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
) {
    let Ok(resolved) = std::fs::canonicalize(path) else {
        return;
    };
    if denied.iter().any(|deny| resolved.starts_with(deny)) || !visited.insert(resolved.clone()) {
        return;
    }
    if !denied.iter().any(|deny| deny.starts_with(&resolved)) {
        grants.push(resolved);
        return;
    }
    let Ok(entries) = std::fs::read_dir(&resolved) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        expand(&entry.path(), denied, grants, visited);
    }
}

#[cfg(not(target_os = "linux"))]
fn confine_and_exec(request: &Request) -> Result<std::convert::Infallible, String> {
    let command = std::iter::once(request.program.as_str())
        .chain(request.arguments.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    Err(format!(
        "there is no Landlock on {}, so `{command}` was not run and {} write path(s) and {} \
         port(s) and {} read denial(s) were not applied; gyr-confine is a Linux binary",
        std::env::consts::OS,
        request.writable.len(),
        request.connect.len(),
        request.deny_read.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_granted_around_a_denied_path_and_not_through_a_symlink() {
        let base = std::env::temp_dir().join(format!("gyr-grants-{}", std::process::id()));
        let home = base.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".config/gyr")).unwrap();
        std::fs::create_dir_all(home.join(".config/helix")).unwrap();
        std::fs::create_dir_all(home.join("code")).unwrap();
        std::fs::write(home.join(".gitconfig"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(home.join(".ssh"), home.join("keys")).unwrap();
        let home = std::fs::canonicalize(&home).unwrap();

        let mut grants = read_grants(
            &home,
            &[
                home.join(".ssh").display().to_string(),
                home.join(".config/gyr").display().to_string(),
            ],
        );
        grants.sort();

        assert_eq!(
            grants,
            [
                home.join(".config/helix"),
                home.join(".gitconfig"),
                home.join("code"),
            ]
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
