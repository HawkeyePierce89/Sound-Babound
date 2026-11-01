//! Logging setup.

use tracing_subscriber::EnvFilter;

/// Filter used when `RUST_LOG` is unset or invalid.
const DEFAULT_FILTER: &str = "info";

/// Builds the filter from `RUST_LOG`, falling back to `info`.
fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Installs the global `tracing` subscriber, writing to stderr. Safe to call
/// more than once: later calls are ignored.
pub fn init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_filter_parses() {
        assert!(DEFAULT_FILTER.parse::<EnvFilter>().is_ok());
    }

    #[test]
    fn init_twice_does_not_panic() {
        init();
        init();
    }
}
