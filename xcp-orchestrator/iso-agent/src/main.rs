//! ISO Agent — handles XO Lite CE, xoa-proxy, and XCP-ng ISO builds.
//!
//! Responsibilities:
//! - Monitor upstream changes for XO Lite CE and xoa-proxy
//! - Dispatch and monitor GitHub Actions builds
//! - Trigger XCP-ng ISO build when components advance
//! - Write status to xcp-iso-agent.status.json
//! - Write version state to iso_agent_version_state.json

use chrono::Utc;
use shared::{
    create_github_client, load_github_token,
    AgentStatus, WorkflowStatus, OrchestratorError,
    ComponentVersionState, IsoVersionState,
    fetch_repo_head_sha,
    fetch_latest_upstream_xolite_tag, fetch_upstream_xolite_version, fetch_xoa_proxy_version,
    create_and_push_tag, locate_tag_triggered_run, query_run_conclusion,
    append_release_matrix_entry,
    XCPNG_TARGET_VERSION,
};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn, debug};

// ── Constants ─────────────────────────────────────────────────────────────────

const STATUS_FILE: &str = "/var/lib/xcp-hl-orchestrator/xcp-iso-agent.status.json";
const VERSION_STATE_FILE: &str = "/var/lib/xcp-hl-orchestrator/iso_agent_version_state.json";

/// FIX #13: hard cap on the component (xolite-ce + xoa-proxy) monitoring loop.
/// If either build hangs indefinitely the agent would loop forever without this.
const COMPONENT_MONITOR_TIMEOUT: Duration = Duration::from_secs(7200); // 2 hours

/// Hard cap on the ISO build monitoring loop.
const ISO_MONITOR_TIMEOUT: Duration = Duration::from_secs(7200); // 2 hours

// ── Version State ─────────────────────────────────────────────────────────────

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
        let temp = Path::new(VERSION_STATE_FILE).with_extension("tmp");
        fs::write(&temp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&temp, VERSION_STATE_FILE)?;
        debug!("ISO agent version state saved");
        Ok(())
    }
}

// ── Bump Decision ─────────────────────────────────────────────────────────────

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
        return Ok(BumpDecision::UpstreamBump { upstream_version: cargo_version });
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
    let upstream_tag = fetch_latest_upstream_xolite_tag(client).await?;
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

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), OrchestratorError> {
    tracing_subscriber::fmt()
        .with_env_filter("iso_agent=info")
        .init();

    info!("Starting ISO Agent...");

    let token = load_github_token()?;
    let client = create_github_client(&token)?;

    let mut status = AgentStatus::new("initialization", WorkflowStatus::InProgress);
    status.write_to_file(STATUS_FILE)?;

    let mut version_state = IsoAgentVersionState::load()?;
    let trigger_time = Utc::now();

    // ── PHASE 1: Evaluate component changes and dispatch builds ───────────────
    info!("PHASE 1: Evaluating component changes and dispatching builds...");

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

    // ── Process XO Lite CE ────────────────────────────────────────────────────
    let xolite_head_sha: String;
    let mut xolite_tag: Option<String> = None;
    let mut xolite_status = WorkflowStatus::Skipped;
    let mut xolite_url = String::new();
    let mut xolite_id: Option<u64> = None;

    match xolite_res {
        Ok((head_sha, BumpDecision::NoChange)) => {
            info!("xolite-ce: no change detected, skipping.");
            xolite_head_sha = head_sha;
        }
        Ok((head_sha, BumpDecision::UpstreamBump { upstream_version })) => {
            let tag = format!("v{}-ce1", upstream_version);
            version_state.xolite_ce.upstream_version = upstream_version;
            version_state.xolite_ce.ce_counter = 1;
            let actual_tag =
                create_and_push_tag(&client, "xolite-ce", &tag, &head_sha).await?;
            let (id, url) =
                locate_tag_triggered_run(&client, "xolite-ce", &actual_tag, trigger_time)
                    .await?;
            xolite_id = Some(id);
            xolite_url = url;
            xolite_status = WorkflowStatus::InProgress;
            xolite_head_sha = head_sha;
            xolite_tag = Some(actual_tag);
        }
        Ok((head_sha, BumpDecision::PatchBump { upstream_version, next_counter })) => {
            let tag = format!("v{}-ce{}", upstream_version, next_counter);
            version_state.xolite_ce.ce_counter = next_counter;
            let actual_tag =
                create_and_push_tag(&client, "xolite-ce", &tag, &head_sha).await?;
            let (id, url) =
                locate_tag_triggered_run(&client, "xolite-ce", &actual_tag, trigger_time)
                    .await?;
            xolite_id = Some(id);
            xolite_url = url;
            xolite_status = WorkflowStatus::InProgress;
            xolite_head_sha = head_sha;
            xolite_tag = Some(actual_tag);
        }
        Err(e) => {
            warn!("xolite-ce bump detection failed: {}. Skipping this run.", e);
            // FIX #18: was `fetch_repo_head_sha(...).unwrap_or_default()` which
            // risked storing "" as xolite_head_sha if that second fetch also failed.
            // An empty SHA would then differ from version_state.last_built_sha on
            // the next run and trigger a spurious rebuild.
            // Use the previously-known good SHA instead — safe because xolite_status
            // stays Skipped and the SHA is never written to version_state in that case.
            xolite_head_sha = version_state.xolite_ce.last_built_sha.clone();
        }
    }

    // ── Process xoa-proxy ─────────────────────────────────────────────────────
    let xoa_head_sha: String;
    let mut xoa_tag: Option<String> = None;
    let mut xoa_status = WorkflowStatus::Skipped;
    let mut xoa_url = String::new();
    let mut xoa_id: Option<u64> = None;

    match xoa_res {
        Ok((head_sha, BumpDecision::NoChange)) => {
            info!("xoa-proxy: no change detected, skipping.");
            xoa_head_sha = head_sha;
        }
        Ok((head_sha, BumpDecision::UpstreamBump { upstream_version })) => {
            let tag = format!("v{}", upstream_version);
            version_state.xoa_proxy.upstream_version = upstream_version;
            version_state.xoa_proxy.ce_counter = 1;
            let actual_tag =
                create_and_push_tag(&client, "xoa-proxy", &tag, &head_sha).await?;
            let (id, url) =
                locate_tag_triggered_run(&client, "xoa-proxy", &actual_tag, trigger_time)
                    .await?;
            xoa_id = Some(id);
            xoa_url = url;
            xoa_status = WorkflowStatus::InProgress;
            xoa_head_sha = head_sha;
            xoa_tag = Some(actual_tag);
        }
        Ok((head_sha, BumpDecision::PatchBump { upstream_version, next_counter })) => {
            let tag = format!("v{}.{}", upstream_version, next_counter);
            version_state.xoa_proxy.ce_counter = next_counter;
            let actual_tag =
                create_and_push_tag(&client, "xoa-proxy", &tag, &head_sha).await?;
            let (id, url) =
                locate_tag_triggered_run(&client, "xoa-proxy", &actual_tag, trigger_time)
                    .await?;
            xoa_id = Some(id);
            xoa_url = url;
            xoa_status = WorkflowStatus::InProgress;
            xoa_head_sha = head_sha;
            xoa_tag = Some(actual_tag);
        }
        Err(e) => {
            warn!("xoa-proxy bump detection failed: {}. Skipping this run.", e);
            // FIX #18: same as xolite — preserve the last known-good SHA
            xoa_head_sha = version_state.xoa_proxy.last_built_sha.clone();
        }
    }

    status.phase = "monitoring".to_string();
    status.status =
        if xolite_status == WorkflowStatus::InProgress || xoa_status == WorkflowStatus::InProgress
        {
            WorkflowStatus::InProgress
        } else {
            WorkflowStatus::Skipped
        };
    status.write_to_file(STATUS_FILE)?;

    // ── PHASE 2: Monitor component builds ────────────────────────────────────
    info!("PHASE 2: Monitoring component build completions...");

    // FIX #13: add a hard deadline so the agent cannot loop forever if a
    //          GitHub Actions run is stuck in "In Progress" indefinitely.
    let component_deadline = Instant::now() + COMPONENT_MONITOR_TIMEOUT;

    loop {
        // FIX #13: check deadline before sleeping so we exit promptly
        if Instant::now() > component_deadline {
            warn!(
                "Component monitoring exceeded {} hour(s), marking timed-out builds.",
                COMPONENT_MONITOR_TIMEOUT.as_secs() / 3600
            );
            if xolite_status == WorkflowStatus::InProgress {
                xolite_status = WorkflowStatus::Timeout;
            }
            if xoa_status == WorkflowStatus::InProgress {
                xoa_status = WorkflowStatus::Timeout;
            }
            status.status = WorkflowStatus::Timeout;
            status.detail = "Component monitoring timed out".to_string();
            status.write_to_file(STATUS_FILE)?;
            break;
        }

        sleep(Duration::from_secs(20)).await;

        if xolite_status == WorkflowStatus::InProgress {
            if let Some(id) = xolite_id {
                let conclusion = query_run_conclusion(&client, "xolite-ce", id).await?;
                xolite_status = match conclusion.as_str() {
                    "success"    => WorkflowStatus::Success,
                    "failure"    => WorkflowStatus::Failure,
                    "timed_out"  => WorkflowStatus::Timeout,
                    "cancelled"  => WorkflowStatus::Aborted,
                    _            => WorkflowStatus::InProgress,
                };
            }
        }

        if xoa_status == WorkflowStatus::InProgress {
            if let Some(id) = xoa_id {
                let conclusion = query_run_conclusion(&client, "xoa-proxy", id).await?;
                xoa_status = match conclusion.as_str() {
                    "success"    => WorkflowStatus::Success,
                    "failure"    => WorkflowStatus::Failure,
                    "timed_out"  => WorkflowStatus::Timeout,
                    "cancelled"  => WorkflowStatus::Aborted,
                    _            => WorkflowStatus::InProgress,
                };
            }
        }

        info!(
            "Component build state — xolite-ce: {} | xoa-proxy: {}",
            xolite_status, xoa_status
        );

        status.timestamp = Utc::now();
        status.status = if xolite_status == WorkflowStatus::InProgress
            || xoa_status == WorkflowStatus::InProgress
        {
            WorkflowStatus::InProgress
        } else if xolite_status == WorkflowStatus::Failure
            || xoa_status == WorkflowStatus::Failure
            || xolite_status == WorkflowStatus::Timeout
            || xoa_status == WorkflowStatus::Timeout
        {
            WorkflowStatus::Failure
        } else {
            WorkflowStatus::Success
        };
        status.write_to_file(STATUS_FILE)?;

        if xolite_status != WorkflowStatus::InProgress
            && xoa_status != WorkflowStatus::InProgress
        {
            break;
        }
    }

    // If any component failed or timed out, abort — no point building an ISO
    let component_failure = xolite_status == WorkflowStatus::Failure
        || xoa_status == WorkflowStatus::Failure
        || xolite_status == WorkflowStatus::Timeout
        || xoa_status == WorkflowStatus::Timeout;

    if component_failure {
        info!("Component failure detected. Aborting ISO build.");
        status.phase = "failed".to_string();
        status.status = WorkflowStatus::Failure;
        status.detail = "Component build failed or timed out; ISO build aborted".to_string();
        if xolite_status == WorkflowStatus::Failure && xolite_id.is_some() {
            status.url = xolite_url;
        } else if xoa_status == WorkflowStatus::Failure && xoa_id.is_some() {
            status.url = xoa_url;
        }
        status.write_to_file(STATUS_FILE)?;
        return Ok(());
    }

    // Persist SHAs only for components that actually built successfully
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

    // ── PHASE 3: ISO generation ───────────────────────────────────────────────
    let xolite_version = version_state.xolite_ce.last_tag.clone();
    let xoa_proxy_version = version_state.xoa_proxy.last_tag.clone();

    let needs_iso_build = version_state.iso.xcpng_version != XCPNG_TARGET_VERSION
        || version_state.iso.last_xolite_tag != xolite_version
        || version_state.iso.last_xoa_proxy_tag != xoa_proxy_version;

    if !needs_iso_build {
        info!("No component changes since last ISO build, skipping.");
        status.phase = "completed".to_string();
        status.status = WorkflowStatus::Skipped;
        status.detail = "No component changes".to_string();
        status.write_to_file(STATUS_FILE)?;
        return Ok(());
    }

    info!("PHASE 3: Triggering custom ISO build...");

    let next_counter = if version_state.iso.xcpng_version == XCPNG_TARGET_VERSION {
        version_state.iso.ce_counter + 1
    } else {
        1
    };
    let iso_tag = format!("v{}-ce{}", XCPNG_TARGET_VERSION, next_counter);

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
            locate_tag_triggered_run(&client, "xcp-ng-ce-iso", &actual_iso_tag, post_iso_trigger)
                .await?;
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
            status.detail = format!("ISO build running: {}", actual_iso_tag);
            status.write_to_file(STATUS_FILE)?;

            // FIX #13: same deadline pattern applied to the ISO monitoring loop
            let iso_deadline = Instant::now() + ISO_MONITOR_TIMEOUT;
            let mut iso_final_status = WorkflowStatus::InProgress;

            loop {
                if Instant::now() > iso_deadline {
                    warn!(
                        "ISO monitoring exceeded {} hour(s), marking timeout.",
                        ISO_MONITOR_TIMEOUT.as_secs() / 3600
                    );
                    iso_final_status = WorkflowStatus::Timeout;
                    status.status = WorkflowStatus::Timeout;
                    status.detail = "ISO build monitoring timed out".to_string();
                    status.write_to_file(STATUS_FILE)?;
                    break;
                }

                sleep(Duration::from_secs(30)).await;

                let conclusion =
                    query_run_conclusion(&client, "xcp-ng-ce-iso", iso_id).await?;

                iso_final_status = match conclusion.as_str() {
                    "success"   => WorkflowStatus::Success,
                    "failure"   => WorkflowStatus::Failure,
                    "timed_out" => WorkflowStatus::Timeout,
                    "cancelled" => WorkflowStatus::Aborted,
                    _           => WorkflowStatus::InProgress,
                };

                info!("ISO build state: {}", iso_final_status);
                status.status = iso_final_status.clone();
                status.write_to_file(STATUS_FILE)?;

                if iso_final_status != WorkflowStatus::InProgress {
                    break;
                }
            }

            if iso_final_status == WorkflowStatus::Success {
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
                status.detail = format!("ISO build successful: {}", actual_iso_tag);
            } else {
                status.phase = "failed".to_string();
                status.detail = format!("ISO build ended with: {}", iso_final_status);
            }
            status.write_to_file(STATUS_FILE)?;
        }
    }

    Ok(())
}
