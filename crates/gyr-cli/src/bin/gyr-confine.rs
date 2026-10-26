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
//!     [--loopback] -- <program> [args]...
//! gyr-confine --check-loopback
//! ```
//!
//! `--loopback` gives the process a network namespace of its own with only
//! `lo` up, so servers can bind and connect on 127.0.0.1 while nothing reaches
//! another host, over TCP or anything else. `--check-loopback` says whether this
//! machine allows that, by trying it.

use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments == ["--check-loopback"] {
        return match isolate_loopback() {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("gyr-confine: {message}");
                ExitCode::FAILURE
            }
        };
    }
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
    /// A network namespace with only loopback, in place of denying TCP.
    loopback: bool,
    program: String,
    arguments: Vec<String>,
}

impl Request {
    fn parse(arguments: &[String]) -> Option<Self> {
        let mut writable = Vec::new();
        let mut connect = Vec::new();
        let mut deny_read = Vec::new();
        let mut loopback = false;
        let mut rest = arguments.iter();
        loop {
            match rest.next()?.as_str() {
                "--allow-write" => writable.push(rest.next()?.clone()),
                "--allow-connect" => connect.push(rest.next()?.parse().ok()?),
                "--deny-read" => deny_read.push(rest.next()?.clone()),
                "--loopback" => loopback = true,
                "--" => break,
                _ => return None,
            }
        }
        let program = rest.next()?.clone();
        Some(Self {
            writable,
            connect,
            deny_read,
            loopback,
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

    // In a namespace holding only loopback there is nowhere else to connect,
    // so TCP is left to the namespace rather than denied outright.
    if request.loopback {
        isolate_loopback()?;
    }

    let ruleset = Ruleset::default()
        // Without this the crate degrades to best effort on an older kernel and
        // enforces whatever it can, silently. The whole point of a floor is that
        // falling below it is an error.
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|error| format!("cannot handle filesystem access: {error}"))?;
    let ruleset = if request.loopback {
        ruleset
    } else {
        ruleset
            .handle_access(AccessNet::from_all(abi))
            .map_err(|error| format!("cannot handle network access: {error}"))?
    };
    let mut ruleset = ruleset
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

    // As Seatbelt allows: much of the toolchain, git among it, opens /dev/null
    // for writing and fails without it.
    if let Ok(null) = PathFd::new("/dev/null") {
        ruleset = ruleset
            .add_rule(PathBeneath::new(null, AccessFs::from_file(abi)))
            .map_err(|error| format!("cannot allow /dev/null: {error}"))?;
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

/// Moves this process into new user and network namespaces, maps its own user
/// and group into them, and brings `lo` up, leaving it no route anywhere else.
///
/// The user namespace is what lets an unprivileged process own a network
/// namespace; mapping the same ids keeps file ownership as it was outside.
#[cfg(target_os = "linux")]
fn isolate_loopback() -> Result<(), String> {
    use nix::sched::CloneFlags;
    use nix::sched::unshare;

    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNET).map_err(|error| {
        format!("cannot create a network namespace ({error}); user namespaces may be disabled")
    })?;
    let write = |path: &str, value: String| {
        std::fs::write(path, value).map_err(|error| format!("cannot write {path}: {error}"))
    };
    write("/proc/self/setgroups", "deny".to_owned())?;
    write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
    write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    loopback_up()
}

/// Sets `lo` up with one `RTM_NEWLINK` request over route netlink. In a new
/// namespace it is always interface 1.
#[cfg(target_os = "linux")]
fn loopback_up() -> Result<(), String> {
    use nix::sys::socket::AddressFamily;
    use nix::sys::socket::MsgFlags;
    use nix::sys::socket::NetlinkAddr;
    use nix::sys::socket::SockFlag;
    use nix::sys::socket::SockProtocol;
    use nix::sys::socket::SockType;
    use nix::sys::socket::recv;
    use nix::sys::socket::sendto;
    use nix::sys::socket::socket;
    use std::os::fd::AsRawFd as _;

    const RTM_NEWLINK: u16 = 16;
    const NLM_F_REQUEST_ACK: u16 = 1 | 4;
    const NLMSG_ERROR: u16 = 2;
    const IFF_UP: u32 = 1;
    const LOOPBACK_INDEX: i32 = 1;

    let socket = socket(
        AddressFamily::Netlink,
        SockType::Raw,
        SockFlag::empty(),
        SockProtocol::NetlinkRoute,
    )
    .map_err(|error| format!("cannot open a netlink socket: {error}"))?;

    // nlmsghdr (len, type, flags, seq, pid), then ifinfomsg (family, pad,
    // type, index, flags, change), in the machine's byte order.
    let mut message = Vec::with_capacity(32);
    message.extend_from_slice(&32_u32.to_ne_bytes());
    message.extend_from_slice(&RTM_NEWLINK.to_ne_bytes());
    message.extend_from_slice(&NLM_F_REQUEST_ACK.to_ne_bytes());
    message.extend_from_slice(&1_u32.to_ne_bytes());
    message.extend_from_slice(&0_u32.to_ne_bytes());
    message.extend_from_slice(&[0, 0, 0, 0]);
    message.extend_from_slice(&LOOPBACK_INDEX.to_ne_bytes());
    message.extend_from_slice(&IFF_UP.to_ne_bytes());
    message.extend_from_slice(&IFF_UP.to_ne_bytes());

    sendto(
        socket.as_raw_fd(),
        &message,
        &NetlinkAddr::new(0, 0),
        MsgFlags::empty(),
    )
    .map_err(|error| format!("cannot ask for lo to come up: {error}"))?;
    let mut reply = [0_u8; 256];
    let read = recv(socket.as_raw_fd(), &mut reply, MsgFlags::empty())
        .map_err(|error| format!("no answer about lo: {error}"))?;
    let field = |at: usize, width: usize| reply.get(at..at + width);
    let kind = field(4, 2).map(|bytes| u16::from_ne_bytes([bytes[0], bytes[1]]));
    let code =
        field(16, 4).map(|bytes| i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
    match (read >= 20, kind, code) {
        (true, Some(NLMSG_ERROR), Some(0)) => Ok(()),
        (true, Some(NLMSG_ERROR), Some(code)) => Err(format!(
            "the kernel refused to bring lo up: {}",
            std::io::Error::from_raw_os_error(-code)
        )),
        _ => Err("an unexpected answer about lo".to_owned()),
    }
}

#[cfg(not(target_os = "linux"))]
fn isolate_loopback() -> Result<(), String> {
    Err(format!(
        "there are no network namespaces on {}",
        std::env::consts::OS
    ))
}

#[cfg(not(target_os = "linux"))]
fn confine_and_exec(request: &Request) -> Result<std::convert::Infallible, String> {
    let command = std::iter::once(request.program.as_str())
        .chain(request.arguments.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    Err(format!(
        "there is no Landlock on {}, so `{command}` was not run and {} write path(s), {} \
         port(s), {} read denial(s) and loopback isolation ({}) were not applied; gyr-confine \
         is a Linux binary",
        std::env::consts::OS,
        request.writable.len(),
        request.connect.len(),
        request.deny_read.len(),
        if request.loopback {
            "asked for"
        } else {
            "not asked for"
        }
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
