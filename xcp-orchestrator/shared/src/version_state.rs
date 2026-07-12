//! Version state management for tracking build versions.

use super::util::write_atomic_json;
use super::OrchestratorError;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Version state for a single component (XO Lite CE, xoa-proxy)
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ComponentVersionState {
    /// Upstream version we last built against (e.g. "0.34.2"). Empty if never built.
    pub upstream_version: String,
    /// Our patch-revision counter against that upstream_version (the "ce" suffix).
    pub ce_counter: u32,
    /// Full tag we last pushed (e.g. "v0.34.2-ce3"). Empty if never built.
    pub last_tag: String,
    /// SHA of our own repo's main branch at the time of last_tag, used to detect
    /// "we changed our patches/spec but upstream didn't move" without re-diffing.
    pub last_built_sha: String,
}

/// Version state for ISO builds
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct IsoVersionState {
    /// XCP-ng base version this edition currently targets (e.g. "8.3").
    pub xcpng_version: String,
    /// Community build counter for that xcpng_version (the "ce" suffix).
    pub ce_counter: u32,
    /// Full tag we last pushed (e.g. "v8.3-ce4"). Empty if never built.
    pub last_tag: String,
    /// Component tags baked into the last ISO build — used to skip rebuilding
    /// when neither component has advanced since.
    pub last_xolite_tag: String,
    pub last_xoa_proxy_tag: String,
}

/// Version state for XOA-HL builds
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct XoaHlVersionState {
    /// Last built SHA of the xoa-hl repository
    pub last_built_sha: String,
    /// Last built tag (if any)
    pub last_tag: String,
}

/// Complete version state for all components
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct VersionState {
    pub xolite_ce: ComponentVersionState,
    pub xoa_proxy: ComponentVersionState,
    pub iso: IsoVersionState,
    #[serde(default)]
    pub xoa_hl: XoaHlVersionState,
}

impl VersionState {
    /// Load version state from file
    pub fn load(path: impl AsRef<Path>) -> Result<Self, OrchestratorError> {
        let path = path.as_ref();
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content).unwrap_or_default())
        } else {
            Ok(Self::default())
        }
    }

    /// Save version state to file atomically
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), OrchestratorError> {
        write_atomic_json(path, self)
    }
}
