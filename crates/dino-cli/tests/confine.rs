//! `dino-confine` on a real kernel: loopback in a namespace of its own where the
//! machine allows one, and no network at all where it does not.

#![cfg(target_os = "linux")]

use std::process::Command;

const CONFINE: &str = env!("CARGO_BIN_EXE_dino-confine");

/// Binds 127.0.0.1, connects to itself and says so; then tries another host.
const PROBE: &str = r#"
import socket
try:
    s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen()
    c = socket.create_connection(("127.0.0.1", s.getsockname()[1]), timeout=2)
    print("loopback ok")
except OSError:
    print("loopback denied")
try:
    socket.create_connection(("1.1.1.1", 80), timeout=3)
    print("outside reached")
except OSError:
    print("outside blocked")
"#;

fn confined(loopback: bool) -> String {
    let directory = std::env::temp_dir();
    let mut command = Command::new(CONFINE);
    command.args(["--allow-write", directory.to_str().unwrap()]);
    if loopback {
        command.arg("--loopback");
    }
    let output = command
        .args(["--", "python3", "-c", PROBE])
        .output()
        .expect("dino-confine runs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_confined_process_gets_loopback_or_nothing_and_never_another_host() {
    let available = Command::new(CONFINE)
        .arg("--check-loopback")
        .status()
        .is_ok_and(|status| status.success());

    if available {
        assert_eq!(confined(true), "loopback ok\noutside blocked\n");
    }
    // Without the namespace, TCP is denied outright, loopback included.
    assert_eq!(confined(false), "loopback denied\noutside blocked\n");
}
