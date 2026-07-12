//! Asynchronous atomic I/O utilities for configuration state files.

use std::path::Path;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use crate::OrchestratorError;

/// Serializes data to a temporary file, forces an OS-level disk flush, 
/// and atomically shifts it into place via rename.
pub async fn write_atomic_json<T>(path: impl AsRef<Path>, data: &T) -> Result<(), OrchestratorError>
where
    T: serde::Serialize,
{
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let temp_path = path.with_extension("tmp");
    let serialized_bytes = serde_json::to_vec_pretty(data)?;

    // 1. Create temporary file descriptor
    let mut file = File::create(&temp_path).await?;

    // 2. Stream out payload contents
    file.write_all(&serialized_bytes).await?;

    // 3. Force synchronization of file data and structural metadata blocks to disk
    file.sync_all().await?;
    drop(file);

    // 4. Perform atomic swap operation
    fs::rename(&temp_path, path).await?;
    Ok(())
}

/// Reads and parses an arbitrary data model from disk asynchronously.
pub async fn load_json_with_default<T>(path: impl AsRef<Path>) -> Result<T, OrchestratorError>
where
    T: serde::de::DeserializeOwned + Default,
{
    let path = path.as_ref();
    if !path.exists() {
        return Ok(T::default());
    }
    let content = fs::read_to_string(path).await?;
    let parsed = serde_json::from_str(&content).unwrap_or_default();
    Ok(parsed)
}