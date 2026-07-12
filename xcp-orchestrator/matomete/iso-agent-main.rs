//! ISO Agent - Handles XO Lite CE, xoa-proxy, and XCP-ng ISO builds
//!
//! Responsibilities:
//! - Monitor upstream changes for XO Lite CE
//! - Monitor changes for xoa-proxy
//! - Build and push XCP-ng ISO when needed
//! - Write status to xcp-iso-agent.status.json
//! - Write version state to iso_agent_version_state.json

use chrono::{DateTime, Utc};
use anyhow::{Context, bail};
use shared::{create_github_client, AgentStatus, WorkflowStatus, OrchestratorError};
use reqwest::Client;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn, error, debug};

/// Status file path for ISO agent
const STATUS_FILE: &str = "/var/lib/xcp-hl-orchestrator/xcp-iso-agent.status.json";

/// Version state file path for ISO agent
const VERSION_STATE_FILE: &str = "/var/lib/xcp-hl-orchestrator/iso_agent_version_state.json";

/// Custom version state for ISO agent
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct IsoAgentVersionState {
    pub xolite_ce: ComponentVersionState,
    pub xoa_proxy: ComponentVersionState,
    pub iso: IsoVersionState,
}

impl IsoAgentVersionState {
    fn load() -> Result<Self, OrchestratorError> {
        if Path::new(VERSION_STATE_FILE).exists() {
            let content = fs::read_to_string(VERSION_STATE_FILE)?;
            Ok(serde_json::from_str(&content).unwrap_or_default())
        } else {
            Ok(Self::default())
        }
    }

    fn save(&self) -> Result<(), OrchestratorError> {
        fs::create_dir_all(Path::new(VERSION_STATE_FILE).parent().unwrap())?;
        let temp_path = Path::new(VERSION_STATE_FILE).with_extension("tmp");
        fs::write(&temp_path, serde_json::to_string_pretty(self)?)?;
        fs::rename(temp_path, VERSION_STATE_FILE)?;
        debug!("Saved ISO agent version state");
        Ok(())
    }
}

enum BumpDecision {
    NoChange,
    UpstreamBump { upstream_version: String },
    PatchBump { upstream_version: String, next_counter: u32 },
}

async fn decide_xoa_proxy_bump(
    client: &reqwest::Client,
    state: &ComponentVersionState,
) -> Result<BumpDecision, OrchestratorError> {
    let cargo_version = fetch_xoa_proxy_version(client).await?;
    let head_sha = fetch_repo_head_sha(client, "xoa-proxy").await?;

    if cargo_version != state.upstream_version {
        return Ok(BumpDecision::UpstreamBump {
            upstream_version: cargo_version,
        });
    }
    if head_sha != state.last_built_sha {
        return Ok(BumpDecision::PatchBump {
            upstream_version: cargo_version,
            next_counter: state.ce_counter + 1,
        });
    }
    Ok(BumpDecision::NoChange)
}

async fn decide_xolite_bump(
    client: &reqwest::Client,
    state: &ComponentVersionState,
) -> Result<BumpDecision, OrchestratorError> {
    let upstream_tag = shared::fetch_latest_upstream_xolite_tag(client).await?;
    let upstream_version = fetch_upstream_xolite_version(client, &upstream_tag).await?;
    let head_sha = fetch_repo_head_sha(client, "xolite-ce").await?;

    if upstream_version != state.upstream_version {
        return Ok(BumpDecision::UpstreamBump { upstream_version });
    }
    if head_sha != state.last_built_sha {
        return Ok(BumpDecision::PatchBump {
            upstream_version,
            next_counter: state.ce_counter + 1,
        });
    }
    Ok(BumpDecision::NoChange)
}

#[tokio::main]
async fn main() -> Result<(), OrchestratorError> {
    tracing_subscriber::fmt()
        .with_env_filter("iso_agent=info")
        .init();

    info!("Starting ISO Agent...");

    // Load GitHub token
    let token = shared::load_github_token()?;
    let client = create_github_client(&token)?;

    // Initialize status
    let mut status = AgentStatus::new("initialization", WorkflowStatus::InProgress);
    status.write_to_file(STATUS_FILE)?;

    // Load version state
    let mut version_state = IsoAgentVersionState::load()?;
    let trigger_time = Utc::now();

    // PHASE 1: Decide component tags and dispatch builds concurrently
    info!("PHASE 1: Evaluating component changes and triggering builds...");

    let xolite_branch = async {
        let head_sha = fetch_repo_head_sha(&client, "xolite-ce").await?;
        let decision = decide_xolite_bump(&client, &version_state.xolite_ce).await?;
        Ok::<(String, BumpDecision), OrchestratorError>((head_sha, decision))
    };
    let xoa_branch = async {
        let head_sha = fetch_repo_head_sha(&client, "xoa-proxy").await?;
        let decision = decide_xoa_proxy_bump(&client, &version_state.xoa_proxy).await?;
        Ok::<(String, BumpDecision), OrchestratorError>((head_sha, decision))
    };

    let (xolite_res, xoa_res) = tokio::join!(xolite_branch, xoa_branch);

    // Process XO Lite CE
    let xolite_head_sha: String;
    let mut xolite_tag: Option<String> = None;
    let mut xolite_status = WorkflowStatus::Skipped;
    let mut xolite_url = String::new();
    let mut xolite_id: Option<u64> = None;

    match xolite_res {
        Ok((head_sha, BumpDecision::NoChange)) => {
            info!("xolite-ce: no upstream or local change detected, skipping build.");
            xolite_head_sha = head_sha;
        }
        Ok((head_sha, BumpDecision::UpstreamBump { upstream_version })) => {
            let tag = format!("v{}-ce1", upstream_version);
            version_state.xolite_ce.upstream_version = upstream_version;
            version_state.xolite_ce.ce_counter = 1;
            let actual_tag = create_and_push_tag(&client, "xolite-ce", &tag, &head_sha).await?;
            let (id, url) = locate_tag_triggered_run(&client, "xolite-ce", &actual_tag, trigger_time).await?;
            xolite_id = Some(id);
            xolite_url = url;
            xolite_status = WorkflowStatus::InProgress;
            xolite_head_sha = head_sha;
            xolite_tag = Some(actual_tag);
        }
        Ok((head_sha, BumpDecision::PatchBump { upstream_version, next_counter })) => {
            let tag = format!("v{}-ce{}", upstream_version, next_counter);
            version_state.xolite_ce.ce_counter = next_counter;
            let actual_tag = create_and_push_tag(&client, "xolite-ce", &tag, &head_sha).await?;
            let (id, url) = locate_tag_triggered_run(&client, "xolite-ce", &actual_tag, trigger_time).await?;
            xolite_id = Some(id);
            xolite_url = url;
            xolite_status = WorkflowStatus::InProgress;
            xolite_head_sha = head_sha;
            xolite_tag = Some(actual_tag);
        }
        Err(e) => {
            warn!("xolite-ce bump detection failed: {}. Skipping build this run.", e);
            xolite_head_sha = fetch_repo_head_sha(&client, "xolite-ce").await.unwrap_or_default();
        }
    }

    // Process xoa-proxy
    let xoa_head_sha: String;
    let mut xoa_tag: Option<String> = None;
    let mut xoa_status = WorkflowStatus::Skipped;
    let mut xoa_url = String::new();
    let mut xoa_id: Option<u64> = None;

    match xoa_res {
        Ok((head_sha, BumpDecision::NoChange)) => {
            info!("xoa-proxy: no version or local change detected, skipping build.");
            xoa_head_sha = head_sha;
        }
        Ok((head_sha, BumpDecision::UpstreamBump { upstream_version })) => {
            let tag = format!("v{}", upstream_version);
            version_state.xoa_proxy.upstream_version = upstream_version;
            version_state.xoa_proxy.ce_counter = 1;
            let actual_tag = create_and_push_tag(&client, "xoa-proxy", &tag, &head_sha).await?;
            let (id, url) = locate_tag_triggered_run(&client, "xoa-proxy", &actual_tag, trigger_time).await?;
            xoa_id = Some(id);
            xoa_url = url;
            xoa_status = WorkflowStatus::InProgress;
            xoa_head_sha = head_sha;
            xoa_tag = Some(actual_tag);
        }
        Ok((head_sha, BumpDecision::PatchBump { upstream_version, next_counter })) => {
            let tag = format!("v{}.{}", upstream_version, next_counter);
            version_state.xoa_proxy.ce_counter = next_counter;
            let actual_tag = create_and_push_tag(&client, "xoa-proxy", &tag, &head_sha).await?;
            let (id, url) = locate_tag_triggered_run(&client, "xoa-proxy", &actual_tag, trigger_time).await?;
            xoa_id = Some(id);
            xoa_url = url;
            xoa_status = WorkflowStatus::InProgress;
            xoa_head_sha = head_sha;
            xoa_tag = Some(actual_tag);
        }
        Err(e) => {
            warn!("xoa-proxy bump detection failed: {}. Skipping build this run.", e);
            xoa_head_sha = fetch_repo_head_sha(&client, "xoa-proxy").await.unwrap_or_default();
        }
    }

    // Update status
    status.phase = "monitoring".to_string();
    status.status = if xolite_status == WorkflowStatus::InProgress || xoa_status == WorkflowStatus::InProgress {
        WorkflowStatus::InProgress
    } else {
        WorkflowStatus::Skipped
    };
    status.write_to_file(STATUS_FILE)?;

    // PHASE 2: Parallel Monitoring loop
    info!("PHASE 2: Awaiting parallel compilation completions...");
    loop {
        sleep(Duration::from_secs(20)).await;

        if xolite_status == WorkflowStatus::InProgress {
            if let Some(id) = xolite_id {
                let conclusion = query_run_conclusion(&client, "xolite-ce", id).await?;
                xolite_status = match conclusion.as_str() {
                    "success" => WorkflowStatus::Success,
                    "failure" => WorkflowStatus::Failure,
                    "timed_out" => WorkflowStatus::Timeout,
                    "cancelled" => WorkflowStatus::Aborted,
                    _ => WorkflowStatus::InProgress,
                };
            }
        }
        if xoa_status == WorkflowStatus::InProgress {
            if let Some(id) = xoa_id {
                let conclusion = query_run_conclusion(&client, "xoa-proxy", id).await?;
                xoa_status = match conclusion.as_str() {
                    "success" => WorkflowStatus::Success,
                    "failure" => WorkflowStatus::Failure,
                    "timed_out" => WorkflowStatus::Timeout,
                    "cancelled" => WorkflowStatus::Aborted,
                    _ => WorkflowStatus::InProgress,
                };
            }
        }

        info!("Current Sync State -> Xolite-ce: {} | Xoa-Proxy: {}", xolite_status, xoa_status);

        // Update status file
        status.timestamp = Utc::now();
        status.status = if xolite_status == WorkflowStatus::InProgress || xoa_status == WorkflowStatus::InProgress {
            WorkflowStatus::InProgress
        } else if xolite_status == WorkflowStatus::Failure || xoa_status == WorkflowStatus::Failure {
            WorkflowStatus::Failure
        } else {
            WorkflowStatus::Success
        };
        status.write_to_file(STATUS_FILE)?;

        if xolite_status != WorkflowStatus::InProgress && xoa_status != WorkflowStatus::InProgress {
            break;
        }
    }

    // Check for failures
    if xolite_status == WorkflowStatus::Failure || xoa_status == WorkflowStatus::Failure {
        info!("Execution failure intercepted. ISO build aborted.");

        // Update final status
        status.phase = "failed".to_string();
        status.status = WorkflowStatus::Failure;
        status.detail = "Component build failed, ISO build aborted".to_string();
        if xolite_status == WorkflowStatus::Failure && xolite_id.is_some() {
            status.url = xolite_url;
        } else if xoa_status == WorkflowStatus::Failure && xoa_id.is_some() {
            status.url = xoa_url;
        }
        status.write_to_file(STATUS_FILE)?;

        return Ok(());
    }

    // Persist version state for successful builds
    if xolite_status == WorkflowStatus::Success {
        if let Some(tag) = xolite_tag {
            version_state.xolite_ce.last_tag = tag;
            version_state.xolite_ce.last_built_sha = xolite_head_sha;
        }
    }

    if xoa_status == WorkflowStatus::Success {
        if let Some(tag) = xoa_tag {
            version_state.xoa_proxy.last_tag = tag;
            version_state.xoa_proxy.last_built_sha = xoa_head_sha;
        }
    }

    version_state.save()?;

    // PHASE 3: ISO generation
    let xolite_version = &version_state.xolite_ce.last_tag;
    let xoa_proxy_version = &version_state.xoa_proxy.last_tag;

    let needs_iso_build = version_state.iso.xcpng_version != XCPNG_TARGET_VERSION
        || version_state.iso.last_xolite_tag != *xolite_version
        || version_state.iso.last_xoa_proxy_tag != *xoa_proxy_version;

    if !needs_iso_build {
        info!("xcp-ng-ce-iso: no component change since last ISO build, skipping.");
        status.phase = "completed".to_string();
        status.status = WorkflowStatus::Skipped;
        status.detail = "No component changes detected".to_string();
        status.write_to_file(STATUS_FILE)?;
    } else {
        info!("PHASE 3: Custom ISO creation...");

        let next_counter = if version_state.iso.xcpng_version == XCPNG_TARGET_VERSION {
            version_state.iso.ce_counter + 1
        } else {
            1
        };
        let iso_tag = format!("v{}-ce{}", XCPNG_TARGET_VERSION, next_counter);

        // Update status
        status.phase = "iso_build".to_string();
        status.status = WorkflowStatus::InProgress;
        status.detail = format!("Building ISO with tag {}", iso_tag);
        status.write_to_file(STATUS_FILE)?;

        let iso_build_result: Result<(u64, String, String), OrchestratorError> = async {
            let iso_head_sha = fetch_repo_head_sha(&client, "xcp-ng-ce-iso").await?;
            let actual_iso_tag =
                create_and_push_tag(&client, "xcp-ng-ce-iso", &iso_tag, &iso_head_sha).await?;

            let post_iso_trigger = Utc::now();
            let (iso_id, iso_url) =
                locate_tag_triggered_run(&client, "xcp-ng-ce-iso", &actual_iso_tag, post_iso_trigger).await?;

            Ok((iso_id, iso_url, actual_iso_tag))
        }
        .await;

        match iso_build_result {
            Err(e) => {
                warn!("xcp-ng-ce-iso build failed to start: {}", e);
                status.phase = "iso_build".to_string();
                status.status = WorkflowStatus::Failure;
                status.detail = format!("Failed to start ISO build: {}", e);
                status.write_to_file(STATUS_FILE)?;
                return Err(e);
            }
            Ok((iso_id, iso_url, actual_iso_tag)) => {
                status.url = iso_url.clone();
                status.detail = format!("ISO build started with tag {}", actual_iso_tag);

                let monitor_result: Result<(), OrchestratorError> = async {
                    loop {
                        sleep(Duration::from_secs(30)).await;
                        let conclusion = query_run_conclusion(&client, "xcp-ng-ce-iso", iso_id).await?;
                        let iso_status = match conclusion.as_str() {
                            "success" => WorkflowStatus::Success,
                            "failure" => WorkflowStatus::Failure,
                            "timed_out" => WorkflowStatus::Timeout,
                            "cancelled" => WorkflowStatus::Aborted,
                            _ => WorkflowStatus::InProgress,
                        };

                        status.status = iso_status.clone();
                        status.write_to_file(STATUS_FILE)?;

                        info!("Current ISO State -> {}", iso_status);

                        if iso_status != WorkflowStatus::InProgress {
                            break;
                        }
                    }
                    Ok(())
                }
                .await;

                if let Err(e) = monitor_result {
                    warn!("Error while monitoring xcp-ng-ce-iso build: {}", e);
                    status.status = WorkflowStatus::Failure;
                    status.detail = format!("Error monitoring ISO build: {}", e);
                    status.write_to_file(STATUS_FILE)?;
                    return Err(e);
                }

                if status.status == WorkflowStatus::Success {
                    version_state.iso.xcpng_version = XCPNG_TARGET_VERSION.to_string();
                    version_state.iso.ce_counter = next_counter;
                    version_state.iso.last_tag = actual_iso_tag.clone();
                    version_state.iso.last_xolite_tag = xolite_version.clone();
                    version_state.iso.last_xoa_proxy_tag = xoa_proxy_version.clone();
                    version_state.save()?;

                    append_release_matrix_entry(
                        &client,
                        &actual_iso_tag,
                        &xolite_version,
                        &version_state.xolite_ce.upstream_version,
                        &xoa_proxy_version,
                    )
                    .await?;

                    status.phase = "completed".to_string();
                    status.detail = format!("ISO build successful with tag {}", actual_iso_tag);
                } else {
                    status.phase = "failed".to_string();
                    status.detail = format!("ISO build failed with tag {}", actual_iso_tag);
                }
                status.write_to_file(STATUS_FILE)?;
            }
        }
    }

    Ok(())
}
