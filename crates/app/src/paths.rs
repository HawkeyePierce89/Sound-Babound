//! Config file locations.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::DEFAULT_CONFIG;

/// Directory name under the OS config dir.
const APP_DIR: &str = "sound-babound";
/// Config file name inside [`config_dir`].
const CONFIG_FILE: &str = "config.toml";

/// `dirs::config_dir()/sound-babound`.
pub fn config_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|dir| dir.join(APP_DIR))
        .context("could not determine the OS config directory")
}

/// `config.toml` inside [`config_dir`].
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

/// Writes the commented default config if the config file is missing.
/// Returns the config file path.
pub fn ensure_default_config() -> Result<PathBuf> {
    let path = config_file()?;
    let created = ensure_default_config_at(&path)
        .with_context(|| format!("failed to create default config {}", path.display()))?;
    if created {
        tracing::info!(path = %path.display(), "created default config");
    }
    Ok(path)
}

/// Writes [`DEFAULT_CONFIG`] to `path` (creating parent directories) unless
/// the file already exists. Never overwrites. Returns `true` if it created
/// the file.
pub fn ensure_default_config_at(path: &Path) -> io::Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(DEFAULT_CONFIG.as_bytes())?;
            file.sync_all()?;
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn config_file_is_inside_config_dir() {
        // The OS config dir exists on every dev/CI machine we target.
        let dir = config_dir().unwrap();
        assert!(dir.ends_with(APP_DIR));
        assert_eq!(config_file().unwrap(), dir.join(CONFIG_FILE));
    }

    #[test]
    fn ensure_default_config_creates_once_and_never_overwrites() {
        let tmp = TempDir::new("ensure-config");
        let path = tmp.path().join("nested").join(CONFIG_FILE);

        assert!(ensure_default_config_at(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);

        fs::write(&path, "master_gain = 0.5\n").unwrap();
        assert!(!ensure_default_config_at(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "master_gain = 0.5\n");
    }
}
