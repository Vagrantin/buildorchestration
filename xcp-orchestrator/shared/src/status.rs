//! Status types and helpers for agent status tracking.

use super::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Status of a workflow or phase
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum WorkflowStatus {
    /// Workflow was skipped (no changes detected)
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
            WorkflowStatus::Unknown(s) => write!(f, "{}", s),
        }
    }
}

impl Default for WorkflowStatus {
    fn default() -> Self {
        WorkflowStatus::Skipped
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
    /// Per-component statuses — the dashboard links each entry's URL as "Logs"
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

    /// Load status from a JSON file
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Option<Self>, OrchestratorError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(path)?;
        let status: AgentStatus = serde_json::from_str(&content)?;
        Ok(Some(status))
    }
}

/// Combined status for both workflows (Workflow 1: XOA-HL/XOA Image, Workflow 2: XO Lite/xoa-proxy/ISO)
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PipelineStatus {
    /// Status of the ISO agent (XO Lite, xoa-proxy, ISO)
    pub iso_agent: Option<AgentStatus>,
    /// Status of the XOA VM agent (XOA-HL)
    pub xoa_vm_agent: Option<AgentStatus>,
    /// Overall pipeline status
    pub overall: WorkflowStatus,
}

impl PipelineStatus {
    /// Create a new pipeline status
    pub fn new() -> Self {
        Self::default()
    }

    /// Update overall status based on agent statuses
    pub fn update_overall(&mut self) {
        let iso_status = self.iso_agent.as_ref().map(|s| &s.status).unwrap_or(&WorkflowStatus::Skipped);
        let xoa_status = self.xoa_vm_agent.as_ref().map(|s| &s.status).unwrap_or(&WorkflowStatus::Skipped);

        self.overall = match (iso_status, xoa_status) {
            (WorkflowStatus::Failure, _) | (_, WorkflowStatus::Failure) => WorkflowStatus::Failure,
            (WorkflowStatus::InProgress, _) | (_, WorkflowStatus::InProgress) => WorkflowStatus::InProgress,
            (WorkflowStatus::Timeout, _) | (_, WorkflowStatus::Timeout) => WorkflowStatus::Timeout,
            (WorkflowStatus::Aborted, _) | (_, WorkflowStatus::Aborted) => WorkflowStatus::Aborted,
            (WorkflowStatus::Success, WorkflowStatus::Success) => WorkflowStatus::Success,
            _ => WorkflowStatus::Skipped,
        };
    }
}

/// Get CSS badge class for a status
pub fn get_badge_class(status: &WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Success => "success",
        WorkflowStatus::Failure => "failure",
        WorkflowStatus::InProgress => "progress",
        WorkflowStatus::Timeout | WorkflowStatus::Aborted => "failure",
        WorkflowStatus::Skipped | WorkflowStatus::Unknown(_) => "progress",
    }
}
