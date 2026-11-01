//! Linux glue.

/// No-op: a Linux binary always keeps the console it was launched from.
pub fn attach_parent_console() {}
