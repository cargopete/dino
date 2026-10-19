/// A `#tag` on an entry, stored lower-case without the hash.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag(String);

pub const BILLABLE: &str = "billable";
pub const NON_BILLABLE: &str = "nobill";

impl Tag {
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        let name = word.strip_prefix('#')?;
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            return None;
        }
        Some(Self(name.to_lowercase()))
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is(&self, name: &str) -> bool {
        self.0 == name
    }
}
