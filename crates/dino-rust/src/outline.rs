//! A short outline of a Rust workspace: each source file and the public items
//! it declares, so a model can find its way without reading every file first.
//! A line scan rather than a parse: it only has to point, not to be exact.

use std::fmt::Write as _;
use std::path::Path;

/// The outline's budget in the system prompt. Past it, files are counted.
const MAX_OUTLINE_BYTES: usize = 4 * 1_024;
/// Items named per file, at most.
const ITEMS_PER_FILE: usize = 12;
const KINDS: [&str; 9] = [
    "fn", "struct", "enum", "trait", "mod", "type", "const", "static", "union",
];
const QUALIFIERS: [&str; 4] = ["async", "unsafe", "extern", "\"C\""];

/// The outline of the workspace at `root`, or `None` when it holds no
/// `Cargo.toml` or no Rust source.
#[must_use]
pub fn outline(root: &Path) -> Option<String> {
    if !root.join("Cargo.toml").is_file() {
        return None;
    }
    let mut files: Vec<(String, Vec<String>)> = ignore::WalkBuilder::new(root)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "rs")
        })
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).ok()?;
            // Build output holds generated Rust even where no ignore file says so.
            if relative
                .components()
                .any(|part| part.as_os_str() == "target")
            {
                return None;
            }
            let relative = relative.display().to_string();
            let text = std::fs::read_to_string(entry.path()).ok()?;
            Some((relative, public_items(&text)))
        })
        .collect();
    if files.is_empty() {
        return None;
    }
    files.sort();

    let mut text = String::new();
    let mut left_out = 0;
    for (path, items) in &files {
        let mut line = path.clone();
        if !items.is_empty() {
            let shown: Vec<&str> = items
                .iter()
                .take(ITEMS_PER_FILE)
                .map(String::as_str)
                .collect();
            let _ = write!(line, ": {}", shown.join(", "));
            if items.len() > ITEMS_PER_FILE {
                let _ = write!(line, ", and {} more", items.len() - ITEMS_PER_FILE);
            }
        }
        if text.len() + line.len() + 1 > MAX_OUTLINE_BYTES {
            left_out += 1;
            continue;
        }
        text.push_str(&line);
        text.push('\n');
    }
    if left_out > 0 {
        let _ = writeln!(text, "({left_out} more files; use list and search)");
    }
    Some(text)
}

/// `kind name` for each public item declared at the start of a line.
fn public_items(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start();
            let rest = rest
                .strip_prefix("pub ")
                .or_else(|| rest.strip_prefix("pub(crate) "))?;
            let mut words = rest
                .split_whitespace()
                .skip_while(|word| QUALIFIERS.contains(word));
            let kind = words.next()?;
            if !KINDS.contains(&kind) {
                return None;
            }
            let name: String = words
                .next()?
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            (!name.is_empty()).then(|| format!("{kind} {name}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::outline;
    use super::public_items;

    #[test]
    fn public_items_are_named_with_their_kind() {
        let source = "pub struct Ledger {\n    pub id: u32,\n}\nfn private() {}\npub async fn \
                      post(entry: Entry) {}\npub(crate) mod parse;\npub enum Kind<T> {}\n";
        assert_eq!(
            public_items(source),
            ["struct Ledger", "fn post", "mod parse", "enum Kind"]
        );
    }

    #[test]
    fn a_workspace_without_a_manifest_has_no_outline() {
        let root = std::env::temp_dir().join(format!("dino-outline-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
        assert_eq!(outline(&root), None);

        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert_eq!(outline(&root).as_deref(), Some("src/lib.rs: fn one\n"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
