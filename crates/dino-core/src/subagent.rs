//! The `agent` tool, which hands a bounded job to a fresh session and returns
//! its conclusion. The runtime lives with whatever builds sessions; the core
//! only knows the name, because consecutive calls to it run concurrently.

pub const AGENT: &str = "agent";
