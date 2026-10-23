//! macOS containment through `sandbox-exec`.
//!
//! Apple deprecated `sandbox-exec` and continues to ship it. When it goes, this
//! file is the only thing that has to change, which is most of the argument for
//! the trait it implements.

use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;

use crate::CREDENTIAL_PATHS;
use crate::Sandbox;
use crate::SandboxError;
use crate::WrappedCommand;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Where a confined child may write, besides the workspace itself.
const WRITABLE_DEVICES: [&str; 1] = ["/dev/null"];

#[derive(Debug)]
pub struct Seatbelt {
    profile: String,
    temp_dir: PathBuf,
    protected: usize,
    credentials_denied: bool,
}

impl Seatbelt {
    /// Builds a profile confining writes to one canonical workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Unavailable`] when `sandbox-exec` is absent, and
    /// [`SandboxError::Profile`] when the root cannot be canonicalised or holds
    /// characters that cannot be written into a profile.
    pub fn new(workspace: &Path) -> Result<Self, SandboxError> {
        Self::protecting(workspace, &[])
    }

    /// As [`Seatbelt::new`], and also denies writes beneath each of these
    /// workspace-relative paths, whether or not they exist yet.
    ///
    /// # Errors
    ///
    /// As [`Seatbelt::new`].
    pub fn protecting(workspace: &Path, protected: &[PathBuf]) -> Result<Self, SandboxError> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::confining(workspace, protected, home.as_deref())
    }

    /// As [`Seatbelt::protecting`], denying credential reads beneath `home`.
    fn confining(
        workspace: &Path,
        protected: &[PathBuf],
        home: Option<&Path>,
    ) -> Result<Self, SandboxError> {
        if !Path::new(SANDBOX_EXEC).is_file() {
            return Err(SandboxError::Unavailable(format!(
                "{SANDBOX_EXEC} is not present"
            )));
        }
        let root = std::fs::canonicalize(workspace).map_err(|error| {
            SandboxError::Profile(format!(
                "cannot resolve workspace {}: {error}",
                workspace.display()
            ))
        })?;
        let temp_dir = root.join(".gyr").join("tmp");
        std::fs::create_dir_all(&temp_dir).map_err(|error| {
            SandboxError::Profile(format!(
                "cannot create sandbox temporary directory {}: {error}",
                temp_dir.display()
            ))
        })?;
        // Canonical, because Seatbelt matches the resolved path and /var and
        // /tmp are symlinks on macOS.
        let home = home.and_then(|home| std::fs::canonicalize(home).ok());
        Ok(Self {
            profile: profile_for(&root, protected, home.as_deref())?,
            temp_dir,
            protected: protected.len(),
            credentials_denied: home.is_some(),
        })
    }

    /// The generated profile, for tests and for `gyr sandbox`.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }
}

impl Sandbox for Seatbelt {
    fn label(&self) -> String {
        let protected = match self.protected {
            0 => String::new(),
            count => format!(", {count} path(s) protected"),
        };
        let credentials = if self.credentials_denied {
            ", credential reads denied"
        } else {
            ""
        };
        format!("workspace (seatbelt: writes confined{protected}{credentials}, network denied)")
    }

    fn confines_writes(&self) -> bool {
        true
    }

    fn denies_network(&self) -> bool {
        true
    }

    fn protects_paths(&self) -> bool {
        self.protected > 0
    }

    fn temp_dir(&self) -> Option<&Path> {
        Some(&self.temp_dir)
    }

    fn wrap(&self, program: &str, arguments: &[String]) -> Result<WrappedCommand, SandboxError> {
        let mut wrapped = vec!["-p".to_owned(), self.profile.clone(), program.to_owned()];
        wrapped.extend_from_slice(arguments);
        Ok(WrappedCommand {
            program: SANDBOX_EXEC.to_owned(),
            arguments: wrapped,
        })
    }
}

/// Renders the profile for one canonical root.
///
/// Reads are allowed everywhere but the credential paths under `home`. See
/// RFC-0009 section 2 for why an allow-list was rejected rather than guessed at.
fn profile_for(
    root: &Path,
    protected: &[PathBuf],
    home: Option<&Path>,
) -> Result<String, SandboxError> {
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (allow process-exec)\n\
         (allow process-fork)\n\
         (allow file-read*)\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow signal (target self))\n",
    );
    // After the read allow, because the last matching rule wins.
    if let Some(home) = home {
        for credential in CREDENTIAL_PATHS {
            let path = home.join(credential);
            let path = path
                .to_str()
                .ok_or_else(|| SandboxError::Profile("home directory is not valid UTF-8".into()))?;
            writeln!(profile, "(deny file-read* (subpath \"{}\"))", escape(path)?)
                .expect("writing to a String cannot fail");
        }
    }
    for device in WRITABLE_DEVICES {
        writeln!(profile, "(allow file-write* (literal \"{device}\"))")
            .expect("writing to a String cannot fail");
    }
    let root = root
        .to_str()
        .ok_or_else(|| SandboxError::Profile("workspace path is not valid UTF-8".into()))?;
    writeln!(
        profile,
        "(allow file-write* (subpath \"{}\"))",
        escape(root)?
    )
    .expect("writing to a String cannot fail");
    // After the allow, because the last matching rule wins. Checked by hand on
    // macOS 26: writing, renaming in, renaming away and deleting were refused.
    for path in protected {
        let path = Path::new(root).join(path);
        let path = path
            .to_str()
            .ok_or_else(|| SandboxError::Profile("protected path is not valid UTF-8".into()))?;
        writeln!(
            profile,
            "(deny file-write* (subpath \"{}\"))",
            escape(path)?
        )
        .expect("writing to a String cannot fail");
    }
    Ok(profile)
}

/// Escapes a path for an SBPL string literal.
///
/// A path is interpolated into a profile that decides what a process may write
/// to, which makes this an injection surface rather than a formatting detail. A
/// stray quote produced a malformed profile when tried by hand; `sandbox-exec`
/// refused to start, which is the good outcome and not one to depend on.
fn escape(value: &str) -> Result<String, SandboxError> {
    if let Some(character) = value.chars().find(|character| character.is_control()) {
        return Err(SandboxError::Profile(format!(
            "workspace path holds a control character ({}), which cannot be confined",
            character.escape_debug()
        )));
    }
    Ok(value
        .chars()
        .flat_map(|character| match character {
            '\\' => vec!['\\', '\\'],
            '"' => vec!['\\', '"'],
            other => vec![other],
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_quote_in_a_path_cannot_close_the_string_literal() {
        assert_eq!(escape(r#"/tmp/a"b"#).unwrap(), r#"/tmp/a\"b"#);
        assert_eq!(escape(r"/tmp/a\b").unwrap(), r"/tmp/a\\b");
        assert_eq!(escape("/tmp/plain").unwrap(), "/tmp/plain");
    }

    #[test]
    fn a_control_character_is_refused_rather_than_escaped() {
        let error = escape("/tmp/a\nb").unwrap_err();

        assert!(error.to_string().contains("control character"), "{error}");
    }

    #[test]
    fn the_profile_denies_by_default_and_writes_only_to_the_root() {
        let profile = profile_for(Path::new("/tmp/workspace"), &[], None).unwrap();

        assert!(profile.starts_with("(version 1)\n(deny default)\n"));
        assert!(profile.contains("(allow file-read*)"));
        assert!(profile.contains(r#"(allow file-write* (subpath "/tmp/workspace"))"#));
        assert!(profile.contains(r#"(allow file-write* (literal "/dev/null"))"#));
        assert!(
            !profile.contains("network"),
            "denying by default is what denies the network; naming it would only \
             invite an allow rule to be added beside it"
        );
    }

    #[test]
    fn protected_paths_are_denied_after_the_workspace_is_allowed() {
        let profile =
            profile_for(Path::new("/tmp/workspace"), &[PathBuf::from("tests")], None).unwrap();

        let allow = profile
            .find(r#"(allow file-write* (subpath "/tmp/workspace"))"#)
            .unwrap();
        let deny = profile
            .find(r#"(deny file-write* (subpath "/tmp/workspace/tests"))"#)
            .unwrap();
        assert!(allow < deny, "the deny must come last to win");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn credentials_under_home_cannot_be_read_but_the_rest_of_home_can() {
        // macOS will not apply a sandbox inside another, so under gyr's own
        // every read would fail and the denials below would pass for nothing.
        let nested = std::process::Command::new(SANDBOX_EXEC)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .status()
            .is_ok_and(|status| !status.success());
        if nested {
            eprintln!("skipped: already inside a sandbox");
            return;
        }
        let base = std::env::temp_dir().join(format!("gyr-cred-{}", std::process::id()));
        let home = base.join("home");
        let workspace = base.join("workspace");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".config/gyr")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "secret").unwrap();
        std::fs::write(home.join(".config/gyr/env"), "secret").unwrap();
        std::fs::write(home.join(".gitconfig"), "fine").unwrap();
        let sandbox = Seatbelt::confining(&workspace, &[], Some(&home)).unwrap();

        let cat = |path: &Path| {
            let wrapped = sandbox
                .wrap("/bin/cat", &[path.display().to_string()])
                .unwrap();
            std::process::Command::new(&wrapped.program)
                .args(&wrapped.arguments)
                .output()
                .unwrap()
        };

        assert!(!cat(&home.join(".ssh/id_ed25519")).status.success());
        assert!(!cat(&home.join(".config/gyr/env")).status.success());
        let allowed = cat(&home.join(".gitconfig"));
        assert_eq!(String::from_utf8_lossy(&allowed.stdout), "fine");
        assert!(sandbox.label().contains("credential reads denied"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrapping_puts_the_profile_before_the_program() {
        let root = std::env::temp_dir();
        let sandbox = Seatbelt::new(&root).unwrap();

        let wrapped = sandbox
            .wrap("cargo", &["check".to_owned(), "--offline".to_owned()])
            .unwrap();

        assert_eq!(wrapped.program, SANDBOX_EXEC);
        assert_eq!(wrapped.arguments[0], "-p");
        assert_eq!(wrapped.arguments[1], sandbox.profile());
        assert_eq!(wrapped.arguments[2], "cargo");
        assert_eq!(wrapped.arguments[3], "check");
        assert_eq!(wrapped.arguments[4], "--offline");
        assert!(sandbox.temp_dir().unwrap().ends_with(".gyr/tmp"));
    }
}
