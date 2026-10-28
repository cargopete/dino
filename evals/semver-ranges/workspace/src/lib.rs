use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Identifier {
    Numeric(u64),
    Alpha(String),
}

#[derive(Debug, Clone)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<Identifier>,
    pub build: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError;

#[derive(Debug, Clone)]
pub struct Range {
    // Replace with whatever representation you need.
    _private: (),
}

impl Version {
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let _ = s;
        todo!()
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let _ = other;
        todo!()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!()
    }
}

impl Range {
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let _ = s;
        todo!()
    }

    pub fn matches(&self, version: &Version) -> bool {
        let _ = version;
        todo!()
    }

    pub fn max_satisfying<'a>(&self, versions: &'a [Version]) -> Option<&'a Version> {
        let _ = versions;
        todo!()
    }

    pub fn min_satisfying<'a>(&self, versions: &'a [Version]) -> Option<&'a Version> {
        let _ = versions;
        todo!()
    }
}
