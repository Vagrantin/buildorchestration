//! Utility functions and helpers.

use super::OrchestratorError;
use serde::Serialize;
use std::fs;
use std::path::Path;

/// Load GitHub token from systemd credential directory.
///
/// Reads `$CREDENTIALS_DIRECTORY/GITHUB_TOKEN` as written by:
///   `LoadCredential=GITHUB_TOKEN:/etc/xcp-hl-credentials/github_token`
pub fn load_github_token() -> Result<String, OrchestratorError> {
    let creds_dir = std::env::var("CREDENTIALS_DIRECTORY").map_err(|_| {
        OrchestratorError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "CREDENTIALS_DIRECTORY is not set — configure systemd LoadCredential=",
        ))
    })?;

    let token_path = Path::new(&creds_dir).join("GITHUB_TOKEN");
    let token = fs::read_to_string(&token_path).map_err(|e| {
        OrchestratorError::Io(std::io::Error::new(
            e.kind(),
            format!("Failed to read GITHUB_TOKEN from {:?}: {}", token_path, e),
        ))
    })?;

    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(OrchestratorError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "GITHUB_TOKEN credential file is empty",
        )));
    }

    Ok(token)
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
