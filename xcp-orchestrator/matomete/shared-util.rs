//! Utility functions and helpers.

use super::OrchestratorError;
use serde::Serialize;
use std::fs;
use std::path::Path;

/// Ensure directory exists
pub fn ensure_dir_exists(path: impl AsRef<Path>) -> Result<(), OrchestratorError> {
    fs::create_dir_all(path.as_ref())?;
    Ok(())
}

/// Atomic file write helper for JSON
pub fn write_atomic_json<T: Serialize>(path: impl AsRef<Path>, data: &T) -> Result<(), OrchestratorError> {
    let path = path.as_ref();
    ensure_dir_exists(path.parent().unwrap_or(path))?;

    let temp_path = path.with_extension("tmp");
    fs::write(&temp_path, serde_json::to_string_pretty(data)?)?;
    fs::rename(temp_path, path)?;

    tracing::debug!("Atomic write to {}", path.display());
    Ok(())
}

/// Load JSON file with default
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
