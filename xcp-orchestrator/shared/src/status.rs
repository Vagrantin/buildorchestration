//! Status types and helpers for agent status tracking.

use super::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Status of a workflow or phase
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub enum WorkflowStatus {
    /// Workflow was skipped (no changes detected)
    #[default]
    Skipped,
    /// Workflow is currently in progress
    InProgress,
    /// Workflow completed successfully
    Success,
    /// Workflow failed
    Failure,
    /// Workflow timed out
    Timeout,
    /// Workflow was aborted
    Aborted,
    /// The change check itself failed, so nothing was decided or built
    CheckFailed,
    /// Unknown status (with custom message)
    Unknown(String),
}

impl std::fmt::Display for WorkflowStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowStatus::Skipped => write!(f, "Skipped"),
            WorkflowStatus::InProgress => write!(f, "In Progress"),
            WorkflowStatus::Success => write!(f, "Success"),
            WorkflowStatus::Failure => write!(f, "Failure"),
            WorkflowStatus::Timeout => write!(f, "Timeout"),
            WorkflowStatus::Aborted => write!(f, "Aborted"),
            WorkflowStatus::CheckFailed => write!(f, "Check failed"),
            WorkflowStatus::Unknown(s) => write!(f, "{}", s),
        }
    }
}

/// Status of one component handled by an agent (e.g. "xolite-ce", "xoa-image"),
/// with a link to its GitHub Actions run or release page.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ComponentStatus {
    pub name: String,
    pub status: WorkflowStatus,
    pub url: String,
}

/// Status information for a single agent
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AgentStatus {
    /// Current phase of the workflow
    pub phase: String,
    /// Current status of the workflow
    pub status: WorkflowStatus,
    /// URL to relevant logs or workflow run
    pub url: String,
    /// Additional details about the status
    pub detail: String,
    /// Timestamp of the last status update
    pub timestamp: DateTime<Utc>,
    /// Per-component statuses, each with a link to its release or run
    #[serde(default)]
    pub components: Vec<ComponentStatus>,
}

impl AgentStatus {
    /// Create a new agent status
    pub fn new(phase: impl Into<String>, status: WorkflowStatus) -> Self {
        Self {
            phase: phase.into(),
            status,
            url: String::new(),
            detail: String::new(),
            timestamp: Utc::now(),
            components: Vec::new(),
        }
    }

    /// Insert or update a component entry by name.
    pub fn set_component(
        &mut self,
        name: impl Into<String>,
        status: WorkflowStatus,
        url: impl Into<String>,
    ) {
        let name = name.into();
        let url = url.into();
        if let Some(existing) = self.components.iter_mut().find(|c| c.name == name) {
            existing.status = status;
            if !url.is_empty() {
                existing.url = url;
            }
        } else {
            self.components.push(ComponentStatus { name, status, url });
        }
    }

    /// Look up a component's (status, url), if recorded.
    pub fn component(&self, name: &str) -> Option<&ComponentStatus> {
        self.components.iter().find(|c| c.name == name)
    }

    /// Write status to a JSON file atomically
    pub fn write_to_file(&self, path: impl AsRef<Path>) -> Result<(), OrchestratorError> {
        let path = path.as_ref();
        std::fs::create_dir_all(path.parent().unwrap_or(path))?;

        // Write to temp file first
        let temp_path = path.with_extension("tmp");
        std::fs::write(&temp_path, serde_json::to_string_pretty(self)?)?;

        // Atomic rename
        std::fs::rename(&temp_path, path)?;

        tracing::debug!("Wrote status to {}", path.display());
        Ok(())
    }
}
