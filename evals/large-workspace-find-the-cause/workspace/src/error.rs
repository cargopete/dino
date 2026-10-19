use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A line of the config could not be read.
    Config { line: usize, message: String },
    /// A config value had the wrong shape for its key.
    Value { key: String, message: String },
    /// A line of the log could not be read.
    Entry { line: usize, message: String },
    UnknownPlugin(String),
    UnknownProject(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config { line, message } => write!(f, "config line {line}: {message}"),
            Error::Value { key, message } => write!(f, "config key {key}: {message}"),
            Error::Entry { line, message } => write!(f, "log line {line}: {message}"),
            Error::UnknownPlugin(name) => write!(f, "no report plugin called {name}"),
            Error::UnknownProject(name) => write!(f, "no entries for project {name}"),
        }
    }
}

impl std::error::Error for Error {}
