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
    fetch_repo_head_sha, fetch_latest_release_ref,
    fetch_latest_upstream_xolite_tag, fetch_pinned_xolite_tag, fetch_upstream_xolite_version,
    fetch_xoa_proxy_version,
    parse_ce_tag, parse_plain_version_tag,
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

/// Consecutive status-poll failures tolerated before giving up. GitHub API
/// blips are routine over a multi-hour monitor; only a sustained outage
/// should abort the run.
const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 5;

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

/// Dev override: force a rebuild even when the version/SHA rules say nothing
/// changed. Forcing a component pushes a fresh -ceN tag (a real release).
#[derive(Default, Clone, Copy)]
struct ForceFlags {
    xolite: bool,
    xoa_proxy: bool,
    iso: bool,
}

fn parse_force_flags() -> Result<ForceFlags, OrchestratorError> {
    let mut force = ForceFlags::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--force" => {
                force.xolite = true;
                force.xoa_proxy = true;
                force.iso = true;
            }
            "--force-xolite" => force.xolite = true,
            "--force-xoa-proxy" => force.xoa_proxy = true,
            "--force-iso" => force.iso = true,
            other => {
                return Err(OrchestratorError::InvalidArgument(format!(
                    "{} (usage: iso-agent [--force] [--force-xolite] [--force-xoa-proxy] [--force-iso])",
                    other
                )));
            }
        }
    }
    Ok(force)
}

async fn decide_xoa_proxy_bump(
    client: &reqwest::Client,
    state: &mut ComponentVersionState,
    force: bool,
) -> Result<BumpDecision, OrchestratorError> {
    let cargo_version = fetch_xoa_proxy_version(client).await?;
    let head_sha = fetch_repo_head_sha(client, "xoa-proxy").await?;

    if !force && cargo_version == state.upstream_version && head_sha == state.last_built_sha {
        return Ok(BumpDecision::NoChange);
    }

    // Local state says rebuild — cross-check the latest release (ground truth)
    // so a lost or stale state file cannot trigger a pointless rebuild.
    if !force
        && latest_release_matches(client, "xoa-proxy", &head_sha, &cargo_version, state, |tag| {
            parse_plain_version_tag(tag)
        })
        .await
    {
        return Ok(BumpDecision::NoChange);
    }

    if cargo_version != state.upstream_version {
        return Ok(BumpDecision::UpstreamBump { upstream_version: cargo_version });
    }
    Ok(BumpDecision::PatchBump {
        upstream_version: cargo_version,
        next_counter: state.ce_counter + 1,
    })
}

async fn decide_xolite_bump(
    client: &reqwest::Client,
    state: &mut ComponentVersionState,
    force: bool,
) -> Result<BumpDecision, OrchestratorError> {
    // Prefer the UPSTREAM_TAG pin committed in the xolite-ce repo — builds only
    // move when that file is bumped, not on every upstream xo-lite release.
    let upstream_tag = match fetch_pinned_xolite_tag(client).await? {
        Some(tag) => {
            info!("xolite-ce: using pinned upstream tag xo-lite-v{}", tag);
            tag
        }
        None => {
            warn!("xolite-ce: no UPSTREAM_TAG pin found, falling back to latest upstream release.");
            fetch_latest_upstream_xolite_tag(client).await?
        }
    };
    let upstream_version = fetch_upstream_xolite_version(client, &upstream_tag).await?;
    let head_sha = fetch_repo_head_sha(client, "xolite-ce").await?;

    if !force && upstream_version == state.upstream_version && head_sha == state.last_built_sha {
        return Ok(BumpDecision::NoChange);
    }

    if !force
        && latest_release_matches(client, "xolite-ce", &head_sha, &upstream_version, state, |tag| {
            parse_ce_tag(tag)
        })
        .await
    {
        return Ok(BumpDecision::NoChange);
    }

    if upstream_version != state.upstream_version {
        return Ok(BumpDecision::UpstreamBump { upstream_version });
    }
    Ok(BumpDecision::PatchBump {
        upstream_version,
        next_counter: state.ce_counter + 1,
    })
}

/// Check whether the repo's latest GitHub release already covers the current
/// HEAD and expected version. If so, backfill `state` from it (making local
/// state self-healing) and return true — nothing needs rebuilding.
async fn latest_release_matches(
    client: &reqwest::Client,
    repo: &str,
    head_sha: &str,
    expected_version: &str,
    state: &mut ComponentVersionState,
    parse_tag: impl Fn(&str) -> Option<(String, u32)>,
) -> bool {
    match fetch_latest_release_ref(client, repo).await {
        Ok(Some((tag, release_sha))) if release_sha == head_sha => {
            match parse_tag(&tag) {
                Some((version, counter)) if version == expected_version => {
                    info!(
                        "{}: latest release {} already matches HEAD, backfilling state.",
                        repo, tag
                    );
                    state.upstream_version = version;
                    state.ce_counter = counter;
                    state.last_tag = tag;
                    state.last_built_sha = head_sha.to_string();
                    true
                }
                _ => false,
            }
        }
        Ok(_) => false,
        Err(e) => {
            warn!("Could not check latest {} release ({}); trusting local state.", repo, e);
            false
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), OrchestratorError> {
    tracing_subscriber::fmt()
        .with_env_filter("iso_agent=info")
        .init();

    info!("Starting ISO Agent...");

    let force = parse_force_flags()?;
    if force.xolite || force.xoa_proxy || force.iso {
        info!(
            "FORCE MODE: xolite={} xoa-proxy={} iso={} — matching skip rules will be bypassed.",
            force.xolite, force.xoa_proxy, force.iso
        );
    }

    let token = load_github_token()?;
    let client = create_github_client(&token)?;

    let mut status = AgentStatus::new("initialization", WorkflowStatus::InProgress);
    status.write_to_file(STATUS_FILE)?;

    let mut version_state = IsoAgentVersionState::load()?;
    let trigger_time = Utc::now();

    // ── PHASE 1: Evaluate component changes and dispatch builds ───────────────
    info!("PHASE 1: Evaluating component changes and dispatching builds...");

    // The decide functions may backfill state from the latest GitHub release,
    // so each branch works on its own clone, merged back after the join.
    let mut xolite_state = version_state.xolite_ce.clone();
    let mut xoa_state = version_state.xoa_proxy.clone();

    let xolite_branch = async {
        let head_sha = fetch_repo_head_sha(&client, "xolite-ce").await?;
        let decision = decide_xolite_bump(&client, &mut xolite_state, force.xolite).await?;
        Ok::<(String, BumpDecision), OrchestratorError>((head_sha, decision))
    };
    let xoa_branch = async {
        let head_sha = fetch_repo_head_sha(&client, "xoa-proxy").await?;
        let decision = decide_xoa_proxy_bump(&client, &mut xoa_state, force.xoa_proxy).await?;
        Ok::<(String, BumpDecision), OrchestratorError>((head_sha, decision))
    };

    let (xolite_res, xoa_res) = tokio::join!(xolite_branch, xoa_branch);
    version_state.xolite_ce = xolite_state;
    version_state.xoa_proxy = xoa_state;

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
    status.set_component("xolite-ce", xolite_status.clone(), xolite_url.clone());
    status.set_component("xoa-proxy", xoa_status.clone(), xoa_url.clone());
    status.write_to_file(STATUS_FILE)?;

    // ── PHASE 2: Monitor component builds ────────────────────────────────────
    info!("PHASE 2: Monitoring component build completions...");

    // FIX #13: add a hard deadline so the agent cannot loop forever if a
    //          GitHub Actions run is stuck in "In Progress" indefinitely.
    let component_deadline = Instant::now() + COMPONENT_MONITOR_TIMEOUT;
    let mut poll_failures: u32 = 0;

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
            status.set_component("xolite-ce", xolite_status.clone(), String::new());
            status.set_component("xoa-proxy", xoa_status.clone(), String::new());
            status.write_to_file(STATUS_FILE)?;
            break;
        }

        sleep(Duration::from_secs(20)).await;

        if xolite_status == WorkflowStatus::InProgress {
            if let Some(id) = xolite_id {
                match query_run_conclusion(&client, "xolite-ce", id).await {
                    Ok(conclusion) => {
                        poll_failures = 0;
                        xolite_status = match conclusion.as_str() {
                            "success"    => WorkflowStatus::Success,
                            "failure"    => WorkflowStatus::Failure,
                            "timed_out"  => WorkflowStatus::Timeout,
                            "cancelled"  => WorkflowStatus::Aborted,
                            _            => WorkflowStatus::InProgress,
                        };
                    }
                    Err(e) => {
                        poll_failures += 1;
                        warn!(
                            "xolite-ce status poll failed ({}/{}): {}",
                            poll_failures, MAX_CONSECUTIVE_POLL_FAILURES, e
                        );
                        if poll_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                            return Err(e);
                        }
                    }
                }
            }
        }

        if xoa_status == WorkflowStatus::InProgress {
            if let Some(id) = xoa_id {
                match query_run_conclusion(&client, "xoa-proxy", id).await {
                    Ok(conclusion) => {
                        poll_failures = 0;
                        xoa_status = match conclusion.as_str() {
                            "success"    => WorkflowStatus::Success,
                            "failure"    => WorkflowStatus::Failure,
                            "timed_out"  => WorkflowStatus::Timeout,
                            "cancelled"  => WorkflowStatus::Aborted,
                            _            => WorkflowStatus::InProgress,
                        };
                    }
                    Err(e) => {
                        poll_failures += 1;
                        warn!(
                            "xoa-proxy status poll failed ({}/{}): {}",
                            poll_failures, MAX_CONSECUTIVE_POLL_FAILURES, e
                        );
                        if poll_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                            return Err(e);
                        }
                    }
                }
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
        status.set_component("xolite-ce", xolite_status.clone(), String::new());
        status.set_component("xoa-proxy", xoa_status.clone(), String::new());
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

    // Seed empty ISO state from the latest xcp-ng-ce-iso release — a lost
    // state file must not force a pointless ISO rebuild when no component
    // advanced this run.
    if version_state.iso.last_tag.is_empty()
        && xolite_status == WorkflowStatus::Skipped
        && xoa_status == WorkflowStatus::Skipped
    {
        match fetch_latest_release_ref(&client, "xcp-ng-ce-iso").await {
            Ok(Some((tag, _sha))) => {
                if let Some((xcpng_version, ce_counter)) = parse_ce_tag(&tag) {
                    info!("Seeding ISO state from latest release {}.", tag);
                    version_state.iso.xcpng_version = xcpng_version;
                    version_state.iso.ce_counter = ce_counter;
                    version_state.iso.last_tag = tag;
                    version_state.iso.last_xolite_tag = xolite_version.clone();
                    version_state.iso.last_xoa_proxy_tag = xoa_proxy_version.clone();
                    version_state.save()?;
                }
            }
            Ok(None) => {}
            Err(e) => warn!(
                "Could not check latest xcp-ng-ce-iso release ({}); trusting local state.",
                e
            ),
        }
    }

    let needs_iso_build = force.iso
        || version_state.iso.xcpng_version != XCPNG_TARGET_VERSION
        || version_state.iso.last_xolite_tag != xolite_version
        || version_state.iso.last_xoa_proxy_tag != xoa_proxy_version;

    if !needs_iso_build {
        info!("No component changes since last ISO build, skipping.");
        status.phase = "completed".to_string();
        status.status = WorkflowStatus::Skipped;
        status.detail = "No component changes".to_string();
        status.set_component("iso", WorkflowStatus::Skipped, String::new());
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
            status.set_component("iso", WorkflowStatus::Failure, String::new());
            status.write_to_file(STATUS_FILE)?;
            return Err(e);
        }
        Ok((iso_id, iso_url, actual_iso_tag)) => {
            status.url = iso_url.clone();
            status.detail = format!("ISO build running: {}", actual_iso_tag);
            status.set_component("iso", WorkflowStatus::InProgress, iso_url.clone());
            status.write_to_file(STATUS_FILE)?;

            // FIX #13: same deadline pattern applied to the ISO monitoring loop
            let iso_deadline = Instant::now() + ISO_MONITOR_TIMEOUT;
            let mut iso_final_status;
            let mut iso_poll_failures: u32 = 0;

            loop {
                if Instant::now() > iso_deadline {
                    warn!(
                        "ISO monitoring exceeded {} hour(s), marking timeout.",
                        ISO_MONITOR_TIMEOUT.as_secs() / 3600
                    );
                    iso_final_status = WorkflowStatus::Timeout;
                    status.status = WorkflowStatus::Timeout;
                    status.detail = "ISO build monitoring timed out".to_string();
                    status.set_component("iso", WorkflowStatus::Timeout, String::new());
                    status.write_to_file(STATUS_FILE)?;
                    break;
                }

                sleep(Duration::from_secs(30)).await;

                let conclusion = match query_run_conclusion(&client, "xcp-ng-ce-iso", iso_id).await
                {
                    Ok(c) => {
                        iso_poll_failures = 0;
                        c
                    }
                    Err(e) => {
                        iso_poll_failures += 1;
                        warn!(
                            "ISO build status poll failed ({}/{}): {}",
                            iso_poll_failures, MAX_CONSECUTIVE_POLL_FAILURES, e
                        );
                        if iso_poll_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                            return Err(e);
                        }
                        continue;
                    }
                };

                iso_final_status = match conclusion.as_str() {
                    "success"   => WorkflowStatus::Success,
                    "failure"   => WorkflowStatus::Failure,
                    "timed_out" => WorkflowStatus::Timeout,
                    "cancelled" => WorkflowStatus::Aborted,
                    _           => WorkflowStatus::InProgress,
                };

                info!("ISO build state: {}", iso_final_status);
                status.status = iso_final_status.clone();
                status.set_component("iso", iso_final_status.clone(), String::new());
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
