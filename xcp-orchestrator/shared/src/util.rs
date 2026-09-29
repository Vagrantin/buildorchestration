//! Utility functions and helpers.

use super::OrchestratorError;
use serde::Serialize;
use std::fs;
use std::path::Path;

/// Load a named credential: from `$CREDENTIALS_DIRECTORY/<name>` under systemd
/// `LoadCredential=`, otherwise from the environment variable `<name>` (Jenkins).
pub fn load_credential(name: &str) -> Result<String, OrchestratorError> {
    let Ok(creds_dir) = std::env::var("CREDENTIALS_DIRECTORY") else {
        return credential_from_env(name, std::env::var(name).ok());
    };

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

/// Jenkins resolves vault references into the environment; an unresolved one must never pass as a token.
fn credential_from_env(name: &str, value: Option<String>) -> Result<String, OrchestratorError> {
    let value = value.map(|v| v.trim().to_string()).unwrap_or_default();
    if value.is_empty() || value.starts_with("pass://") {
        return Err(OrchestratorError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "{name} is not available: set CREDENTIALS_DIRECTORY (systemd) or a resolved {name} environment variable (Jenkins)"
            ),
        )));
    }
    Ok(value)
}

/// Load the GitHub token: systemd `LoadCredential=GITHUB_TOKEN:...`, or `GITHUB_TOKEN` from Jenkins.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_credential_is_trimmed() {
        assert_eq!(credential_from_env("GITHUB_TOKEN", Some(" ghp_x\n".into())).unwrap(), "ghp_x");
    }

    #[test]
    fn missing_empty_or_unresolved_env_credential_is_an_error() {
        assert!(credential_from_env("GITHUB_TOKEN", None).is_err());
        assert!(credential_from_env("GITHUB_TOKEN", Some("  ".into())).is_err());
        assert!(credential_from_env("GITHUB_TOKEN", Some("pass://xcp-hl-prod/github_token/password".into())).is_err());
    }
}
