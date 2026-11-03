//! What a Rust project's own files say about building it: the edition a file
//! is written in, and the toolchain it is pinned to.

use std::path::Path;
use std::path::PathBuf;

use dino_core::ToolError;

/// The edition of the package `file` belongs to, from the nearest manifest
/// with a `[package]` table beneath `root`. None is Cargo's default, 2015.
///
/// # Errors
///
/// Returns an error when a manifest on the way cannot be parsed.
pub fn edition_of(root: &Path, file: &Path) -> Result<Option<String>, ToolError> {
    let path = root.join(file);
    for directory in path.ancestors().skip(1) {
        if !directory.starts_with(root) {
            break;
        }
        let Some(package) = read_manifest(&directory.join("Cargo.toml"))?
            .and_then(|manifest| manifest.get("package").cloned())
        else {
            continue;
        };
        return Ok(match package.get("edition") {
            Some(toml::Value::String(edition)) => Some(edition.clone()),
            Some(toml::Value::Table(_)) => {
                read_manifest(&root.join("Cargo.toml"))?.and_then(|workspace| {
                    workspace
                        .get("workspace")?
                        .get("package")?
                        .get("edition")?
                        .as_str()
                        .map(ToOwned::to_owned)
                })
            }
            _ => None,
        });
    }
    Ok(None)
}

/// A manifest as a table, or None when there is no file to read.
///
/// # Errors
///
/// Returns an error when the file exists and is not TOML.
pub fn read_manifest(path: &Path) -> Result<Option<toml::Table>, ToolError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|error| ToolError::new(format!("cannot read {}: {error}", path.display())))
}

/// The channel a `rust-toolchain.toml` pins, and where rustup is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    pub channel: String,
    /// The file that named it, as a person would find it.
    pub file: PathBuf,
    /// None when rustup cannot be found, so the pin cannot be honoured.
    pub rustup: Option<PathBuf>,
}

impl Toolchain {
    /// The pin rustup would honour for a command run in `root`: the nearest
    /// toolchain file in it or above it, as rustup searches.
    #[must_use]
    pub fn find(root: &Path) -> Option<Self> {
        root.ancestors().find_map(|directory| {
            ["rust-toolchain.toml", "rust-toolchain"]
                .iter()
                .find_map(|name| {
                    let file = directory.join(name);
                    let channel = channel_in(&std::fs::read_to_string(&file).ok()?)?;
                    Some(Self {
                        channel,
                        file,
                        rustup: rustup(),
                    })
                })
        })
    }

    /// `program` and `arguments` as run under the pinned channel. `rustup run`
    /// rather than the toolchain's own binary, because rustup also puts that
    /// toolchain's rustc first on the child's PATH.
    #[must_use]
    pub fn wrap(&self, program: &str, arguments: Vec<String>) -> (String, Vec<String>) {
        let Some(rustup) = &self.rustup else {
            return (program.to_owned(), arguments);
        };
        let mut wrapped = vec!["run".to_owned(), self.channel.clone(), program.to_owned()];
        wrapped.extend(arguments);
        (rustup.display().to_string(), wrapped)
    }

    /// What a tool result says about the toolchain it ran.
    #[must_use]
    pub fn label(&self, root: &Path) -> String {
        let file = self.file.strip_prefix(root).unwrap_or(&self.file).display();
        match self.rustup {
            Some(_) => format!("{} (from {file}, through rustup)", self.channel),
            None => format!(
                "the first on PATH: {file} pins {}, but rustup was not found",
                self.channel
            ),
        }
    }
}

/// The channel in a toolchain file: TOML with `[toolchain] channel`, or the
/// older form holding only the channel's name.
fn channel_in(text: &str) -> Option<String> {
    let channel = match toml::from_str::<toml::Table>(text) {
        Ok(table) => table.get("toolchain")?.get("channel")?.as_str()?.to_owned(),
        Err(_) => text.trim().to_owned(),
    };
    let plain = !channel.is_empty()
        && !channel.starts_with('-')
        && channel
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character));
    plain.then_some(channel)
}

fn rustup() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join("rustup"));
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .map(|home| home.join("bin").join("rustup"));
    on_path.chain(home).find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::Toolchain;
    use super::channel_in;
    use super::edition_of;
    use std::path::Path;
    use std::path::PathBuf;

    #[test]
    fn a_toolchain_file_names_its_channel_in_either_form() {
        let pinned = "# why\n[toolchain]\nchannel = \"1.95.0\"\ncomponents = [\"rustfmt\"]\n";
        assert_eq!(channel_in(pinned).as_deref(), Some("1.95.0"));
        assert_eq!(
            channel_in("nightly-2026-10-01\n").as_deref(),
            Some("nightly-2026-10-01")
        );
        assert_eq!(channel_in("[toolchain]\npath = \"/opt/rust\"\n"), None);
        assert_eq!(channel_in("--help"), None);
    }

    #[test]
    fn a_pinned_command_runs_through_rustup_and_says_so() {
        let pinned = Toolchain {
            channel: "1.95.0".into(),
            file: PathBuf::from("/work/rust-toolchain.toml"),
            rustup: Some(PathBuf::from("/home/me/.cargo/bin/rustup")),
        };
        let (program, arguments) = pinned.wrap("cargo", vec!["test".into()]);
        assert_eq!(program, "/home/me/.cargo/bin/rustup");
        assert_eq!(arguments, ["run", "1.95.0", "cargo", "test"]);
        assert_eq!(
            pinned.label(Path::new("/work")),
            "1.95.0 (from rust-toolchain.toml, through rustup)"
        );

        let stranded = Toolchain {
            rustup: None,
            ..pinned
        };
        assert_eq!(
            stranded.wrap("cargo", vec!["test".into()]),
            ("cargo".to_owned(), vec!["test".to_owned()])
        );
        assert!(
            stranded
                .label(Path::new("/work"))
                .contains("rustup was not found")
        );
    }

    #[test]
    fn the_edition_comes_from_the_nearest_package_or_its_workspace() {
        let root = std::env::temp_dir().join(format!("dino-edition-{}", std::process::id()));
        std::fs::create_dir_all(root.join("core/src")).unwrap();
        std::fs::create_dir_all(root.join("old/src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"core\", \"old\"]\n[workspace.package]\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("core/Cargo.toml"),
            "[package]\nname = \"core\"\nedition.workspace = true\n",
        )
        .unwrap();
        std::fs::write(root.join("old/Cargo.toml"), "[package]\nname = \"old\"\n").unwrap();

        let edition = |file: &str| edition_of(&root, Path::new(file)).unwrap();
        assert_eq!(edition("core/src/lib.rs").as_deref(), Some("2021"));
        assert_eq!(edition("old/src/lib.rs"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
