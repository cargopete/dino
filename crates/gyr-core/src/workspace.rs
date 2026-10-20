//! Resolving a model-supplied path against one workspace root.
//!
//! This lives in the core because more than one tool crate needs it, and two
//! implementations of one security check is how they come to disagree.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::ToolError;

/// A canonical directory that model-supplied paths are resolved against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot {
    root: PathBuf,
}

impl WorkspaceRoot {
    /// Canonicalises an existing directory.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the path cannot be canonicalised or is not a
    /// directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|error| {
            ToolError::new(format!(
                "cannot resolve workspace root {}: {error}",
                root.as_ref().display()
            ))
        })?;
        if !root.is_dir() {
            return Err(ToolError::new(format!(
                "workspace root is not a directory: {}",
                root.display()
            )));
        }
        Ok(Self { root })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Resolves an existing path inside the root.
    ///
    /// The path must be relative, non-empty and free of parent, root or
    /// platform-prefix components. It is then canonicalised, which follows
    /// symbolic links, and the result must still lie beneath the root.
    ///
    /// This is a filesystem fence, not a process sandbox: a directory component
    /// replaced by another process between this check and the use of its result
    /// is still a window. RFC-0009 owns the operating-system boundary.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the path is not relative, does not exist, or
    /// resolves outside the root.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ToolError> {
        let relative = checked_relative(path)?;
        let resolved = std::fs::canonicalize(self.root.join(relative))
            .map_err(|error| ToolError::new(format!("cannot resolve {path}: {error}")))?;
        if !resolved.starts_with(&self.root) {
            return Err(ToolError::new(format!(
                "resolved path escapes the workspace: {path}"
            )));
        }
        Ok(resolved)
    }

    /// Resolves a file that may not exist yet, for writing.
    ///
    /// The deepest existing ancestor is canonicalised and must lie beneath the
    /// root, so a symbolic link to a directory elsewhere cannot be written
    /// through. The rest of the path does not exist, so it cannot be a link.
    ///
    /// # Errors
    ///
    /// As [`WorkspaceRoot::resolve`], and when the target exists but is not a
    /// file.
    pub fn resolve_writable(&self, path: &str) -> Result<PathBuf, ToolError> {
        let relative = checked_relative(path)?;
        let mut existing = self.root.join(relative);
        let mut missing = Vec::new();
        while std::fs::symlink_metadata(&existing).is_err() {
            let Some(name) = existing.file_name() else {
                break;
            };
            missing.push(name.to_owned());
            existing.pop();
        }
        let mut resolved = std::fs::canonicalize(&existing)
            .map_err(|error| ToolError::new(format!("cannot resolve {path}: {error}")))?;
        if !resolved.starts_with(&self.root) {
            return Err(ToolError::new(format!(
                "resolved path escapes the workspace: {path}"
            )));
        }
        resolved.extend(missing.iter().rev());
        if resolved.is_dir() {
            return Err(ToolError::new(format!("path is a directory: {path}")));
        }
        Ok(resolved)
    }

    /// Resolves an existing file inside the root.
    ///
    /// # Errors
    ///
    /// As [`WorkspaceRoot::resolve`], and when the target is not a file.
    pub fn resolve_file(&self, path: &str) -> Result<PathBuf, ToolError> {
        let resolved = self.resolve(path)?;
        if !resolved.is_file() {
            return Err(ToolError::new(format!("path is not a file: {path}")));
        }
        Ok(resolved)
    }

    /// Resolves an existing directory inside the root.
    ///
    /// # Errors
    ///
    /// As [`WorkspaceRoot::resolve`], and when the target is not a directory.
    pub fn resolve_directory(&self, path: &str) -> Result<PathBuf, ToolError> {
        let resolved = self.resolve(path)?;
        if !resolved.is_dir() {
            return Err(ToolError::new(format!("path is not a directory: {path}")));
        }
        Ok(resolved)
    }

    /// Renders a resolved path relative to the root, for display and for the
    /// subject a session rule is keyed on.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the path is not beneath the root, which would
    /// mean a caller resolved it by some other route.
    pub fn relative(&self, path: &Path) -> Result<String, ToolError> {
        path.strip_prefix(&self.root)
            .map(|relative| relative.to_string_lossy().into_owned())
            .map_err(|_| ToolError::new(format!("path escaped the workspace: {}", path.display())))
    }
}

/// Checks a model-supplied path lexically: relative, non-empty, and with no
/// component that could climb out.
fn checked_relative(path: &str) -> Result<&Path, ToolError> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ToolError::new(format!(
            "path must be relative and remain inside the workspace: {path}"
        )));
    }
    Ok(relative)
}
