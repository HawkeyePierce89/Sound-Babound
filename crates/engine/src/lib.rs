//! Audio engine for Sound-Babound.
//!
//! This crate owns decoding, the realtime mixer, the output sink and the
//! device supervisor that rebuilds the output stream when the default device
//! changes. It has no UI dependencies.
//!
//! The public modules are added incrementally; this is the skeleton.

/// Crate version, exposed so the app can log which engine it runs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
