//! Shared types, utilities, and helpers for XCP-HL Orchestrator and its agents.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

pub mod github;
pub mod ollama;
pub mod status;
pub mod storage;
pub mod util;
pub mod version_state;
pub use github::*;
pub use ollama::*;
pub use status::*;
pub use util::*;
pub use version_state::*;

// ── Constants ──────────────────────────────────────────────────────────────

/// GitHub organization owner for all repositories
pub const OWNER: &str = "Vagrantin";

/// Default branch for all repositories
pub const DEFAULT_BRANCH: &str = "main";

/// Documentation repository name
pub const DOCS_REPO: &str = "xcp-hl";

/// Path to releases data file in docs repo
pub const RELEASES_DATA_PATH: &str = "docs/_data/releases.yml";

/// Target XCP-ng base release version
pub const XCPNG_TARGET_VERSION: &str = "8.3";

/// Base directory for all orchestrator state files
pub const STATE_DIR: &str = "/var/lib/xcp-hl-orchestrator";

/// History file path
pub const HISTORY_FILE: &str = "/var/lib/xcp-hl-orchestrator/history.json";

/// Ollama API endpoint
pub const OLLAMA_URL: &str = "http://localhost:11434/api/generate";

/// Ollama model for diagnostics
pub const MODEL_NAME: &str = "qwen3-coder:30b";

/// Dashboard output directory
pub const TARGET_REPORT_DIR: &str = "/var/www/html/orchestrator";

// ── Error Types ────────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum OrchestratorError {
    #[error("GitHub API error ({0}): {1}")]
    GitHubApi(String, String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error")]
    Json(#[from] serde_json::Error),

    #[error("Base64 decode error: {0}")]
    Base64Decode(String),

    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("FromUTF8 error: {0}")]
    FromUtf8(#[from] std::string::FromUtf8Error),

    #[error("Timeout waiting for {0}")]
    Timeout(String),

    #[error("Tag creation failed: {0}")]
    TagCreation(String),

    #[error("Workflow run not found: {0}")]
    WorkflowRunNotFound(String),

    #[error("Ollama error: {0}")]
    OllamaError(String),

    #[error("Invalid version format: {0}")]
    VersionFormat(String),

    #[error("Header value error: {0}")]
    HeaderValueError(String),
}

// ── GitHub Types ────────────────────────────────────────────────────────────

/// GitHub workflow run response
#[derive(Deserialize, Debug, Clone)]
pub struct GHRun {
    pub id: u64,
    pub html_url: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub created_at: DateTime<Utc>,
    pub head_branch: Option<String>,
}

/// GitHub workflow runs list response
#[derive(Deserialize, Debug)]
pub struct GHRunsResponse {
    pub workflow_runs: Vec<GHRun>,
}

/// GitHub job response
#[derive(Deserialize, Debug)]
pub struct GHJob {
    pub id: u64,
    pub conclusion: Option<String>,
}

/// GitHub jobs list response
#[derive(Deserialize, Debug)]
pub struct GHJobsResponse {
    pub jobs: Vec<GHJob>,
}

/// GitHub file content response
#[derive(Deserialize, Debug)]
pub struct GHFileContent {
    pub content: String,
    pub sha: String,
}

/// GitHub release response
#[derive(Deserialize, Debug)]
pub struct GHRelease {
    pub tag_name: String,
    pub assets: Vec<serde_json::Value>,
}
