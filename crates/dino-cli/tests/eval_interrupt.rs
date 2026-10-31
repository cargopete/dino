//! An eval stopped by a signal ends the processes its cases started.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

const MANIFEST: &str = "[workspace]\n\
     [package]\n\
     name = \"looping\"\n\
     version = \"0.1.0\"\n\
     edition = \"2021\"\n";

/// Records where it runs, then never finishes.
const HIDDEN: &str = "#[test]\n\
     fn spins() {\n\
     let pid = std::process::id().to_string();\n\
     std::fs::write(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/spinning.pid\"), pid).unwrap();\n\
     loop {\n\
     std::hint::spin_loop();\n\
     }\n\
     }\n";

fn corpus(root: &Path) -> PathBuf {
    let case = root.join("evals").join("looping");
    for tree in ["workspace", "reference"] {
        fs::create_dir_all(case.join(tree).join("src")).unwrap();
        fs::write(case.join(tree).join("Cargo.toml"), MANIFEST).unwrap();
        fs::write(case.join(tree).join("src/lib.rs"), "").unwrap();
    }
    fs::create_dir_all(case.join("hidden/tests")).unwrap();
    fs::write(case.join("hidden/tests/spins.rs"), HIDDEN).unwrap();
    fs::write(
        case.join("case.toml"),
        "name = \"looping\"\nprompt = \"Nothing.\"\nmax_turns = 1\n\n[expect]\nhidden_tests = true\n",
    )
    .unwrap();
    root.join("evals")
}

fn find_pid(directory: &Path) -> Option<String> {
    for entry in fs::read_dir(directory).ok()?.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if let Some(pid) = find_pid(&path) {
                return Some(pid);
            }
        } else if path.file_name().is_some_and(|name| name == "spinning.pid") {
            return fs::read_to_string(path).ok();
        }
    }
    None
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn an_interrupted_eval_leaves_no_test_running() {
    let root = std::env::temp_dir().join(format!("dino-interrupt-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let corpus = corpus(&root);
    let scratch = root.join("scratch");

    for signal in ["-INT", "-TERM"] {
        let _ = fs::remove_dir_all(&scratch);
        let mut dino = Command::new(env!("CARGO_BIN_EXE_dino"))
            .arg("eval")
            .arg("--calibrate")
            .arg("--plain")
            .arg("--corpus")
            .arg(&corpus)
            .arg("--scratch")
            .arg(&scratch)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let pid = loop {
            if let Some(pid) = find_pid(&scratch) {
                break pid;
            }
            if dino.try_wait().unwrap().is_some() {
                let output = dino.wait_with_output().unwrap();
                panic!(
                    "dino exited before the test started: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "the looping test never started"
            );
            std::thread::sleep(Duration::from_millis(100));
        };

        let signalled = Command::new("kill")
            .args([signal, &dino.id().to_string()])
            .status()
            .unwrap();
        assert!(signalled.success());
        let status = dino.wait().unwrap();

        // A killed process can linger briefly as a zombie until it is reaped.
        let gone = (0..50).any(|_| {
            let found = alive(&pid);
            if found {
                std::thread::sleep(Duration::from_millis(100));
            }
            !found
        });
        if !gone {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
        }
        assert!(
            gone,
            "after {signal} the test {pid} outlived dino, which exited {status}"
        );
        assert!(!status.success(), "an interrupted eval reported success");
    }
    let _ = fs::remove_dir_all(&root);
}
