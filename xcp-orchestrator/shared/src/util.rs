//! Utility functions and helpers.

use super::OrchestratorError;
use serde::Serialize;
use std::fs;
use std::path::Path;

/// Load a named credential from the systemd credentials directory
/// (`$CREDENTIALS_DIRECTORY/<name>`, as written by a unit's `LoadCredential=`).
pub fn load_credential(name: &str) -> Result<String, OrchestratorError> {
    let creds_dir = std::env::var("CREDENTIALS_DIRECTORY").map_err(|_| {
        OrchestratorError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "CREDENTIALS_DIRECTORY is not set — configure systemd LoadCredential=",
        ))
    })?;

    let path = Path::new(&creds_dir).join(name);
    let value = fs::read_to_string(&path).map_err(|e| {
        OrchestratorError::Io(std::io::Error::new(
            e.kind(),
            format!("Failed to read {} from {:?}: {}", name, path, e),
        ))
    })?;

    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(OrchestratorError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} credential file is empty", name),
        )));
    }

    Ok(value)
}

/// Load GitHub token from systemd credential directory.
///
/// Reads `$CREDENTIALS_DIRECTORY/GITHUB_TOKEN` as written by:
///   `LoadCredential=GITHUB_TOKEN:/etc/xcp-hl-credentials/github_token`
pub fn load_github_token() -> Result<String, OrchestratorError> {
    load_credential("GITHUB_TOKEN")
}

/// Ensure directory exists, creating it and all parents if necessary.
pub fn ensure_dir_exists(path: impl AsRef<Path>) -> Result<(), OrchestratorError> {
    fs::create_dir_all(path.as_ref())?;
    Ok(())
}

/// Atomically write `data` as pretty-printed JSON to `path`.
///
/// Writes to a `.tmp` sibling first, then renames — safe against partial writes.
pub fn write_atomic_json<T: Serialize>(
    path: impl AsRef<Path>,
    data: &T,
) -> Result<(), OrchestratorError> {
    let path = path.as_ref();
    ensure_dir_exists(path.parent().unwrap_or(path))?;

    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, serde_json::to_string_pretty(data)?)?;
    fs::rename(&temp_path, path)?;

    tracing::debug!("Atomic write to {}", path.display());
    Ok(())
}

/// Load JSON from `path`, returning `T::default()` if the file does not exist.
/// Malformed JSON also falls back to `default()`.
pub fn load_json_with_default<T: serde::de::DeserializeOwned + Default>(
    path: impl AsRef<Path>,
) -> Result<T, OrchestratorError> {
    let path = path.as_ref();
    if path.exists() {
        let content = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content).unwrap_or_default())
    } else {
        Ok(T::default())
    }
}
