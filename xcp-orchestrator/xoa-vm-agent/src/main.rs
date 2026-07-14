//! XOA VM Agent - Replaces setup-xoa-builder.sh
//!
//! Builds XOA-HL (Xen Orchestra Appliance - Home Lab Edition) XVA images.
//!
//! Workflow:
//!  1. Check if rebuild needed (HEAD SHA vs last_built_sha)
//!  2. Trigger GitHub Actions workflow and wait for RPM build to complete
//!  3. Validate prerequisites (Packer, plugin, disk, ports)
//!  4. Sync repository
//!  5. Resolve dynamic values (ISO checksum, RPM URL, credentials)
//!  6. Generate temporary build files (inst.ks, almalinux-build.json)
//!  7. Run packer validate + build
//!  8. Locate generated XVA
//!  9. Create GitHub Release and upload XVA
//! 10. Persist version state
//! 11. Write final status

use anyhow::{Context, Result, bail};

use chrono::{DateTime, Utc};
use shared::{
    AgentStatus, WorkflowStatus,
    create_github_client, load_github_token,
    fetch_repo_head_sha, fetch_releases, fetch_tag_commit_sha, ReleaseInfo,
    locate_dispatch_triggered_run, query_run_conclusion,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::fs as async_fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener; // FIX #14: real port check via bind attempt
use tokio::process::Command as AsyncCommand;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

// ── Constants ────────────────────────────────────────────────────────────────

const STATUS_FILE: &str = "/var/lib/xcp-hl-orchestrator/xoa-vm-agent.status.json";
/// Infrastructure config (non-secret) — installed by deploy.sh from
/// xoa-vm-agent/build.config.sample. Missing file = baked-in defaults.
const BUILD_CONFIG_FILE: &str = "/etc/xcp-orchestrator/build.config";
const VERSION_STATE_FILE: &str = "/var/lib/xcp-hl-orchestrator/xoa_agent_version_state.json";
const REPO_DIR: &str = "/var/lib/xcp-hl-orchestrator/repos/build-xoa-hl";
const BUILD_DIR: &str = "/var/lib/xcp-hl-orchestrator/build/xoa-hl";
const OUTPUT_DIR: &str = "/var/lib/xcp-hl-orchestrator/output/xoa-hl";

/// Full "owner/repo" path — used directly in API URLs that do not go through
/// the shared helpers (which prepend OWNER themselves).
const XOA_HL_REPO: &str = "Vagrantin/xoa-hl";
const BUILD_XOA_HL_REPO: &str = "Vagrantin/build-xoa-hl";

/// Workflow file that builds the RPM and creates a GitHub Release.
const XOA_HL_WORKFLOW_FILE: &str = "build-xoa.yml";

/// Tag prefix distinguishing this agent's VM-image releases (xoa-image-{date}-{sha7})
/// from the RPM releases created by the workflow (v{version}_{sha}).
const IMAGE_TAG_PREFIX: &str = "xoa-image-";

const ALMALINUX_VERSION: &str = "9";
const ALMALINUX_ISO_URL: &str =
    "https://repo.almalinux.org/almalinux/9/isos/x86_64/AlmaLinux-9-latest-x86_64-minimal.iso";

/// 100 GB minimum free space for build + output artefacts.
const REQUIRED_DISK_SPACE_BYTES: u64 = 100 * 1024 * 1024 * 1024;

/// Ports Packer's embedded HTTP server may bind to (kickstart delivery).
/// All must be free at build time.
const REQUIRED_PORTS: &[u16] = &[8000, 8001, 8002, 8003, 8004, 8005, 8006, 8007, 8008, 8009, 9000];

/// Hard timeout for the full Packer build.
const PACKER_TIMEOUT: Duration = Duration::from_secs(3600);

/// Grace period between SIGTERM and SIGKILL when Packer times out.
const PACKER_SIGTERM_GRACE: Duration = Duration::from_secs(30);

/// Hard timeout waiting for the xoa-hl GA workflow to complete.
const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(3600);

// ── Version State ─────────────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct XoaHlVersionState {
    pub last_built_sha: String,
    pub last_tag: String, // FIX #12: was never written — now updated in Phase 10
    pub last_built_at: Option<DateTime<Utc>>,
}

impl XoaHlVersionState {
    fn load() -> Result<Self> {
        if Path::new(VERSION_STATE_FILE).exists() {
            let content = std::fs::read_to_string(VERSION_STATE_FILE)
                .context("Failed to read XOA-HL version state")?;
            Ok(serde_json::from_str(&content).unwrap_or_default())
        } else {
            Ok(Self::default())
        }
    }

    fn save(&self) -> Result<()> {
        let path = Path::new(VERSION_STATE_FILE);
        std::fs::create_dir_all(path.parent().unwrap())
            .context("Failed to create state directory")?;
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, serde_json::to_string_pretty(self)?)
            .context("Failed to write version state temp file")?;
        std::fs::rename(&temp, path).context("Failed to rename version state file")?;
        debug!("XOA-HL version state saved");
        Ok(())
    }
}

// ── Build Config ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BuildConfig {
    xcpng_ip: String,
    xcpng_user: String,
    xcpng_password: String,
    sr_name: String,
    vm_network_name: String,
    vm_name: String,
    vm_disk_size_mb: u32,
    vm_memory_mb: u32,
    almalinux_root_password: String,
    almalinux_iso_url: String,
    almalinux_iso_checksum: String,
    xoa_hl_rpm_url: String,
    xe_guest_utilities_url: String,
    xe_guest_utilities_xenstore_url: String,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            xcpng_ip: "192.168.1.10".to_string(),
            xcpng_user: "root".to_string(),
            xcpng_password: String::new(),
            sr_name: "Local storage".to_string(),
            vm_network_name: "Pool-wide network associated with eth0".to_string(),
            vm_name: "xoa-almalinux".to_string(),
            vm_disk_size_mb: 10000,
            vm_memory_mb: 2048,
            almalinux_root_password: String::new(),
            almalinux_iso_url: ALMALINUX_ISO_URL.to_string(),
            almalinux_iso_checksum: String::new(),
            xoa_hl_rpm_url: String::new(),
            xe_guest_utilities_url:
                "https://github.com/xenserver/xe-guest-utilities/releases/download/v10.0.0/xe-guest-utilities-10.0.0-1.x86_64.rpm"
                    .to_string(),
            xe_guest_utilities_xenstore_url:
                "https://github.com/xenserver/xe-guest-utilities/releases/download/v10.0.0/xe-guest-utilities-xenstore-10.0.0-1.x86_64.rpm"
                    .to_string(),
        }
    }
}

impl BuildConfig {
    /// Defaults overlaid with /etc/xcp-orchestrator/build.config when present.
    /// A missing file is non-fatal (baked-in defaults keep working); a file
    /// that exists but fails to parse is fatal — a half-applied config
    /// pointing at the wrong host is worse than stopping.
    fn load() -> Result<Self> {
        let mut config = Self::default();
        match std::fs::read_to_string(BUILD_CONFIG_FILE) {
            Ok(content) => {
                apply_build_config(&mut config, &content)
                    .with_context(|| format!("Failed to parse {}", BUILD_CONFIG_FILE))?;
                info!(
                    "Build config loaded from {}: xcpng_ip={}, xcpng_user={}, sr_name={:?}, \
                     network={:?}, vm_name={}, disk={}MB, memory={}MB",
                    BUILD_CONFIG_FILE,
                    config.xcpng_ip,
                    config.xcpng_user,
                    config.sr_name,
                    config.vm_network_name,
                    config.vm_name,
                    config.vm_disk_size_mb,
                    config.vm_memory_mb,
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!(
                    "No {} found — using baked-in default build config. \
                     Run deploy.sh to install one from build.config.sample.",
                    BUILD_CONFIG_FILE
                );
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to read {}", BUILD_CONFIG_FILE));
            }
        }
        Ok(config)
    }
}

/// Overlay shell-style KEY="VALUE" lines onto a BuildConfig. Blank lines and
/// `#` comments are skipped, surrounding quotes are stripped (the file stays
/// `source`-able from a shell). Unknown keys warn but don't fail; secrets are
/// deliberately not accepted here (LoadCredential only).
fn apply_build_config(config: &mut BuildConfig, content: &str) -> Result<()> {
    for (lineno, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("line {}: expected KEY=VALUE, got {:?}", lineno + 1, raw))?;
        let key = key.trim();
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);

        let parse_mb = |v: &str| {
            v.parse::<u32>()
                .with_context(|| format!("line {}: {} must be a number, got {:?}", lineno + 1, key, v))
        };

        match key {
            "XCPNG_IP" => config.xcpng_ip = value.to_string(),
            "XCPNG_USER" => config.xcpng_user = value.to_string(),
            "SR_NAME" => config.sr_name = value.to_string(),
            "VM_NETWORK_NAME" => config.vm_network_name = value.to_string(),
            "VM_NAME" => config.vm_name = value.to_string(),
            "VM_DISK_SIZE_MB" => config.vm_disk_size_mb = parse_mb(value)?,
            "VM_MEMORY_MB" => config.vm_memory_mb = parse_mb(value)?,
            "ALMALINUX_ISO_URL" => config.almalinux_iso_url = value.to_string(),
            "XE_GUEST_UTILITIES_URL" => config.xe_guest_utilities_url = value.to_string(),
            "XE_GUEST_UTILITIES_XENSTORE_URL" => {
                config.xe_guest_utilities_xenstore_url = value.to_string()
            }
            _ => warn!("build.config line {}: unknown key {:?} ignored", lineno + 1, key),
        }
    }
    Ok(())
}

// ── Main Workflow ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("xoa_vm_agent=info,warn")
        .init();

    info!("Starting XOA VM Agent...");

    let mut status = AgentStatus::new("initialization", WorkflowStatus::InProgress);
    status.write_to_file(STATUS_FILE)?;

    let mut version_state = XoaHlVersionState::load()?;
    let token = load_github_token().context("Failed to load GitHub token")?;
    let client = create_github_client(&token)?;

    // ── PHASE 1: Determine if rebuild is needed ──────────────────────────────
    info!("PHASE 1: Checking if rebuild is needed...");
    status.phase = "phase_1_check_rebuild".to_string();
    status.write_to_file(STATUS_FILE)?;

    let repo_head_sha = fetch_repo_head_sha(&client, "xoa-hl")
        .await
        .context("Failed to fetch xoa-hl HEAD SHA")?;

    // A skip requires a *published image* for HEAD, not just unchanged code:
    // the workflow's RPM release and this agent's XVA image release are
    // separate artefacts, and the image is the one this agent exists to ship.
    if !version_state.last_built_sha.is_empty()
        && repo_head_sha == version_state.last_built_sha
        && version_state.last_tag.starts_with(IMAGE_TAG_PREFIX)
    {
        info!(
            "No changes since image {} was built (SHA: {}), skipping.",
            version_state.last_tag,
            &repo_head_sha[..7]
        );
        status.status = WorkflowStatus::Skipped;
        status.detail = format!("No changes (SHA: {})", &repo_head_sha[..7]);
        status.write_to_file(STATUS_FILE)?;
        return Ok(());
    }

    // Local state is empty or untrustworthy — check the published releases
    // (ground truth). An xoa-image-* release with an XVA asset for HEAD means
    // everything is done; a current RPM release without one means only the
    // workflow can be skipped and the image must still be built.
    let short_sha = repo_head_sha[..7.min(repo_head_sha.len())].to_string();
    let mut rpm_is_current = false;
    match fetch_releases(&client, "xoa-hl", 30).await {
        Ok(releases) => {
            if let Some(image) = releases.iter().find(|r| is_image_release_for(r, &short_sha)) {
                info!(
                    "Image {} already published for HEAD (SHA: {}), skipping.",
                    image.tag_name, short_sha
                );
                version_state.last_built_sha = repo_head_sha.clone();
                version_state.last_tag = image.tag_name.clone();
                version_state.last_built_at = Some(Utc::now());
                version_state.save()?;
                status.status = WorkflowStatus::Skipped;
                status.detail =
                    format!("Image {} already published (SHA: {})", image.tag_name, short_sha);
                status.set_component("xoa-hl", WorkflowStatus::Skipped, String::new());
                status.set_component("xoa-image", WorkflowStatus::Success, image.html_url.clone());
                status.write_to_file(STATUS_FILE)?;
                return Ok(());
            }

            // No image for HEAD — does the newest RPM release already cover it?
            if let Some(rpm) = releases.iter().find(|r| !r.tag_name.starts_with(IMAGE_TAG_PREFIX)) {
                match fetch_tag_commit_sha(&client, "xoa-hl", &rpm.tag_name).await {
                    Ok(sha) if sha == repo_head_sha => {
                        info!(
                            "RPM release {} matches HEAD (SHA: {}) — skipping workflow, building missing image.",
                            rpm.tag_name, short_sha
                        );
                        rpm_is_current = true;
                        status.set_component("xoa-hl", WorkflowStatus::Skipped, rpm.html_url.clone());
                    }
                    Ok(_) => {}
                    Err(e) => warn!(
                        "Could not resolve RPM release tag {} ({}); proceeding with full build.",
                        rpm.tag_name, e
                    ),
                }
            }
        }
        Err(e) => warn!(
            "Could not list xoa-hl releases ({}); proceeding with full build.",
            e
        ),
    }

    // ── PHASE 2: Trigger GA workflow and wait ────────────────────────────────
    if rpm_is_current {
        info!("PHASE 2: Skipped — RPM release already covers HEAD.");
    } else {
        info!("PHASE 2: Triggering xoa-hl build workflow...");
        status.phase = "phase_2_trigger_workflow".to_string();
        status.detail = "Dispatching xoa-hl GitHub Actions workflow".to_string();
        status.write_to_file(STATUS_FILE)?;

        let (run_id, run_url) = match trigger_xoa_hl_workflow(&client).await {
            Ok(r) => r,
            Err(e) => {
                error!("Workflow dispatch failed: {}", e);
                status.status = WorkflowStatus::Failure;
                status.detail = format!("Workflow dispatch failed: {}", e);
                status.set_component("xoa-hl", WorkflowStatus::Failure, String::new());
                status.write_to_file(STATUS_FILE)?;
                return Err(e);
            }
        };

        status.url = run_url.clone();
        status.detail = format!("Waiting for workflow: {}", run_url);
        status.set_component("xoa-hl", WorkflowStatus::InProgress, run_url.clone());
        status.write_to_file(STATUS_FILE)?;

        match wait_for_workflow(&client, run_id, &run_url, WORKFLOW_TIMEOUT).await {
            Ok(()) => {
                info!("xoa-hl workflow completed successfully");
                status.set_component("xoa-hl", WorkflowStatus::Success, run_url.clone());
            }
            Err(e) => {
                error!("xoa-hl workflow failed: {}", e);
                status.status = WorkflowStatus::Failure;
                status.detail = format!("Workflow failed: {}", e);
                status.set_component("xoa-hl", WorkflowStatus::Failure, run_url.clone());
                status.write_to_file(STATUS_FILE)?;
                return Err(e);
            }
        }
    }

    // ── PHASE 3: Validate prerequisites ──────────────────────────────────────
    info!("PHASE 3: Validating prerequisites...");
    status.phase = "phase_3_validate_prerequisites".to_string();
    status.write_to_file(STATUS_FILE)?;

    if let Err(e) = validate_prerequisites().await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Prerequisite check failed: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 4: Sync repository ──────────────────────────────────────────────
    info!("PHASE 4: Synchronizing repository...");
    status.phase = "phase_4_sync_repo".to_string();
    status.write_to_file(STATUS_FILE)?;

    if let Err(e) = sync_repository().await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Repository sync failed: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 5: Resolve dynamic values ──────────────────────────────────────
    info!("PHASE 5: Resolving dynamic values...");
    status.phase = "phase_5_resolve_values".to_string();
    status.write_to_file(STATUS_FILE)?;

    let mut config = match BuildConfig::load() {
        Ok(c) => c,
        Err(e) => {
            status.status = WorkflowStatus::Failure;
            status.detail = format!("Build config invalid: {}", e);
            status.write_to_file(STATUS_FILE)?;
            return Err(e);
        }
    };
    if let Err(e) = resolve_dynamic_values(&client, &mut config).await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Value resolution failed: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 6: Generate build files ────────────────────────────────────────
    info!("PHASE 6: Generating build files...");
    status.phase = "phase_6_generate_files".to_string();
    status.write_to_file(STATUS_FILE)?;

    let build_dir = PathBuf::from(BUILD_DIR);
    if let Err(e) = generate_build_files(&build_dir, &config).await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Build file generation failed: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 7: Run Packer ───────────────────────────────────────────────────
    info!(
        "PHASE 7: Running Packer (timeout: {} min)...",
        PACKER_TIMEOUT.as_secs() / 60
    );
    status.phase = "phase_7_run_packer".to_string();
    status.detail = "Running packer validate + build".to_string();
    status.write_to_file(STATUS_FILE)?;

    if let Err(e) = run_packer(&build_dir).await {
        error!("Packer build failed: {}", e);
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Packer error: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 8: Locate XVA ───────────────────────────────────────────────────
    info!("PHASE 8: Locating generated XVA...");
    status.phase = "phase_8_locate_xva".to_string();
    status.write_to_file(STATUS_FILE)?;

    let xva_path = match locate_xva().await {
        Ok(p) => p,
        Err(e) => {
            status.status = WorkflowStatus::Failure;
            status.detail = format!("XVA not found: {}", e);
            status.write_to_file(STATUS_FILE)?;
            return Err(e);
        }
    };
    info!("XVA located: {}", xva_path.display());

    // ── PHASE 9: Create release and upload XVA ────────────────────────────────
    // FIX #11: was calling upload_to_github_release() which assumed a release
    //          already existed. Now we explicitly create (or reuse) one first.
    info!("PHASE 9: Creating GitHub Release and uploading XVA...");
    status.phase = "phase_9_upload_asset".to_string();
    status.write_to_file(STATUS_FILE)?;

    let image_tag = generate_image_tag(&repo_head_sha);
    let image_name = format!("XOA HomeLab Edition - {}", image_tag);

    let (upload_url, release_url) =
        match create_github_release(&client, &image_tag, &image_name, &repo_head_sha).await {
            Ok(u) => u,
            Err(e) => {
                status.status = WorkflowStatus::Failure;
                status.detail = format!("Release creation failed: {}", e);
                status.set_component("xoa-image", WorkflowStatus::Failure, String::new());
                status.write_to_file(STATUS_FILE)?;
                return Err(e);
            }
        };

    status.set_component("xoa-image", WorkflowStatus::InProgress, release_url.clone());
    status.write_to_file(STATUS_FILE)?;

    if let Err(e) = upload_asset(&client, &upload_url, &xva_path).await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Asset upload failed: {}", e);
        status.set_component("xoa-image", WorkflowStatus::Failure, release_url.clone());
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    status.set_component("xoa-image", WorkflowStatus::Success, release_url.clone());

    // ── PHASE 10: Persist version state ──────────────────────────────────────
    info!("PHASE 10: Persisting version state...");
    status.phase = "phase_10_persist_state".to_string();

    version_state.last_built_sha = repo_head_sha.clone();
    version_state.last_tag = image_tag.clone(); // FIX #12: was never set
    version_state.last_built_at = Some(Utc::now());
    version_state.save().context("Failed to persist version state")?;

    // ── PHASE 11: Write final status ──────────────────────────────────────────
    info!("PHASE 11: Finalizing...");
    status.phase = "phase_11_finalize".to_string();
    status.status = WorkflowStatus::Success;
    status.detail = format!(
        "Build complete. Tag: {}. XVA: {}",
        image_tag,
        xva_path.display()
    );
    status.timestamp = Utc::now();
    status.write_to_file(STATUS_FILE)?;

    info!("XOA VM Agent completed successfully! Tag: {}", image_tag);
    Ok(())
}

// ── PHASE 2 helpers ───────────────────────────────────────────────────────────

/// Dispatch the xoa-hl build workflow and wait for the run to appear in the API.
/// Returns `(run_id, html_url)`.
async fn trigger_xoa_hl_workflow(client: &reqwest::Client) -> Result<(u64, String)> {
    let url = format!(
        "https://api.github.com/repos/{}/actions/workflows/{}/dispatches",
        XOA_HL_REPO, XOA_HL_WORKFLOW_FILE,
    );

    let trigger_time = Utc::now();
    let payload = serde_json::json!({ "ref": "main" });

    let res = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .context("Failed to POST workflow dispatch")?;

    // workflow_dispatch returns 204 No Content on success
    if res.status() != reqwest::StatusCode::NO_CONTENT {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("workflow_dispatch returned {} — {}", code, body);
    }

    info!("Workflow dispatched, waiting for run to appear...");

    // FIX (P0-1): workflow_dispatch runs are filed under event=workflow_dispatch,
    // not event=push, and carry no head_branch/tag to match against. Using
    // locate_tag_triggered_run here (event=push filter) meant this call always
    // timed out after 180s. locate_dispatch_triggered_run polls the correct
    // event type and matches purely on created_at >= trigger_time.
    let (run_id, run_url) =
        locate_dispatch_triggered_run(client, "xoa-hl", trigger_time)
            .await
            .context("Failed to locate triggered workflow run")?;

    info!("Workflow run started: {}", run_url);
    Ok((run_id, run_url))
}

/// Poll a run until it completes or `timeout` elapses.
/// Consecutive status-poll failures tolerated before giving up — GitHub API
/// blips are routine over a long monitor and must not abort the build.
const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 5;

async fn wait_for_workflow(
    client: &reqwest::Client,
    run_id: u64,
    run_url: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut poll_failures: u32 = 0;

    loop {
        if Instant::now() > deadline {
            bail!(
                "Timed out after {:?} waiting for xoa-hl workflow ({})",
                timeout,
                run_url
            );
        }

        sleep(Duration::from_secs(30)).await;

        let conclusion = match query_run_conclusion(client, "xoa-hl", run_id).await {
            Ok(c) => {
                poll_failures = 0;
                c
            }
            Err(e) => {
                poll_failures += 1;
                warn!(
                    "xoa-hl status poll failed ({}/{}): {}",
                    poll_failures, MAX_CONSECUTIVE_POLL_FAILURES, e
                );
                if poll_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                    return Err(e).context("Failed to query workflow conclusion");
                }
                continue;
            }
        };

        info!("xoa-hl workflow: {}", conclusion);

        match conclusion.as_str() {
            "success" | "Success" => return Ok(()),
            "In Progress" => continue,
            other => bail!(
                "xoa-hl workflow ended with '{}' ({})",
                other,
                run_url
            ),
        }
    }
}

// ── PHASE 3: Validate Prerequisites ──────────────────────────────────────────

async fn validate_prerequisites() -> Result<()> {
    // FIX #16: was using blocking std::process::Command throughout.
    //          All checks now use AsyncCommand so the executor is not blocked.

    // Check Packer binary
    info!("Checking Packer...");
    let packer_out = AsyncCommand::new("packer")
        .arg("version")
        .output()
        .await
        .context("Packer binary not found — install from https://developer.hashicorp.com/packer/downloads")?;

    if !packer_out.status.success() {
        bail!(
            "Packer version check failed: {}",
            String::from_utf8_lossy(&packer_out.stderr)
        );
    }
    info!("Packer: {}", String::from_utf8_lossy(&packer_out.stdout).trim());

    // Check xenserver plugin
    info!("Checking xenserver Packer plugin...");
    let plugin_out = AsyncCommand::new("packer")
        .arg("plugins")
        .arg("installed")
        .output()
        .await
        .context("Failed to list Packer plugins")?;

    if !String::from_utf8_lossy(&plugin_out.stdout).contains("xenserver") {
        bail!(
            "XCP-ng Packer plugin not installed.\n\
             Run: packer plugins install github.com/ddelnano/xenserver"
        );
    }
    info!("xenserver plugin: present");

    // Check disk space
    info!("Checking disk space...");
    let df_out = AsyncCommand::new("df")
        .arg("-k")
        .arg(BUILD_DIR)
        .output()
        .await
        .context("Failed to run df")?;

    let df_text = String::from_utf8_lossy(&df_out.stdout);
    if let Some(line) = df_text.lines().nth(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            let avail_kb: u64 = parts[3].parse().unwrap_or(0);
            let avail_bytes = avail_kb * 1024;
            if avail_bytes < REQUIRED_DISK_SPACE_BYTES {
                bail!(
                    "Insufficient disk space — required {}GB, have {}GB",
                    REQUIRED_DISK_SPACE_BYTES / (1024 * 1024 * 1024),
                    avail_bytes / (1024 * 1024 * 1024),
                );
            }
            info!(
                "Disk: {}GB available",
                avail_bytes / (1024 * 1024 * 1024)
            );
        }
    }

    // FIX #14: was a stub that unconditionally logged "port available".
    //          Now attempts a real bind; if the port is already in use the bind fails.
    info!("Checking required ports...");
    let mut blocked: Vec<u16> = Vec::new();
    for &port in REQUIRED_PORTS {
        match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(_listener) => debug!("Port {} free", port),
            // listener drops here, releasing the port immediately
            Err(_) => blocked.push(port),
        }
    }
    if !blocked.is_empty() {
        bail!(
            "Required ports already in use: {:?}\n\
             Packer needs these free for its kickstart HTTP server.",
            blocked
        );
    }
    info!("All required ports are available");

    Ok(())
}

// ── PHASE 4: Sync repository ──────────────────────────────────────────────────

async fn sync_repository() -> Result<()> {
    let repo_path = Path::new(REPO_DIR);
    info!("Syncing {}", repo_path.display());

    if !repo_path.exists() {
        let status = AsyncCommand::new("git")
            .args(["clone", &format!("https://github.com/{}", BUILD_XOA_HL_REPO)])
            .arg(repo_path)
            .status()
            .await
            .context("git clone failed")?;
        if !status.success() {
            bail!("git clone exited with {:?}", status.code());
        }
    } else {
        let fetch = AsyncCommand::new("git")
            .arg("fetch")
            .current_dir(repo_path)
            .status()
            .await
            .context("git fetch failed")?;
        if !fetch.success() {
            bail!("git fetch exited with {:?}", fetch.code());
        }

        let reset = AsyncCommand::new("git")
            .args(["reset", "--hard", "origin/main"])
            .current_dir(repo_path)
            .status()
            .await
            .context("git reset failed")?;
        if !reset.success() {
            bail!("git reset exited with {:?}", reset.code());
        }
    }

    info!("Repository synchronized");
    Ok(())
}

// ── PHASE 5: Resolve dynamic values ──────────────────────────────────────────

async fn resolve_dynamic_values(
    client: &reqwest::Client,
    config: &mut BuildConfig,
) -> Result<()> {
    // Credentials come exclusively from systemd LoadCredential — never from env vars set manually
    let creds_dir = std::env::var("CREDENTIALS_DIRECTORY").map_err(|_| {
        anyhow::anyhow!(
            "CREDENTIALS_DIRECTORY not set — configure systemd LoadCredential= \
             for XCPNG_PASSWORD and ALMALINUX_ROOT_PASSWORD"
        )
    })?;
    let creds = std::path::PathBuf::from(creds_dir);

    config.xcpng_password = std::fs::read_to_string(creds.join("XCPNG_PASSWORD"))
        .context("XCPNG_PASSWORD credential missing")?
        .trim()
        .to_string();

    config.almalinux_root_password =
        std::fs::read_to_string(creds.join("ALMALINUX_ROOT_PASSWORD"))
            .context("ALMALINUX_ROOT_PASSWORD credential missing")?
            .trim()
            .to_string();

    info!("Resolving AlmaLinux ISO checksum...");
    config.almalinux_iso_checksum = resolve_almalinux_checksum(client)
        .await
        .context("Failed to resolve AlmaLinux checksum")?;
    info!("Checksum: {}", config.almalinux_iso_checksum);

    info!("Resolving xoa-hl RPM URL...");
    config.xoa_hl_rpm_url = resolve_xoa_hl_rpm_url(client)
        .await
        .context("Failed to resolve xoa-hl RPM URL")?;
    info!("RPM URL: {}", config.xoa_hl_rpm_url);

    Ok(())
}

async fn resolve_almalinux_checksum(client: &reqwest::Client) -> Result<String> {
    let iso_filename = "AlmaLinux-9-latest-x86_64-minimal.iso";
    let base = "https://repo.almalinux.org/almalinux/9/isos/x86_64";

    // Try BSD-style CHECKSUM first
    let checksum_url = format!("{}/CHECKSUM", base);
    if let Ok(res) = client.get(&checksum_url).send().await {
        if res.status().is_success() {
            let body = res.text().await?;
            for line in body.lines() {
                if line.contains(iso_filename) {
                    // BSD format: "SHA256 (filename) = <hash>"
                    if let Some(hash) = line.split_whitespace().last() {
                        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                            return Ok(format!("sha256:{}", hash));
                        }
                    }
                }
            }
        }
    }

    // Fall back to GNU-style SHA256SUMS
    let sha256_url = format!("{}/SHA256SUMS", base);
    if let Ok(res) = client.get(&sha256_url).send().await {
        if res.status().is_success() {
            let body = res.text().await?;
            for line in body.lines() {
                if line.starts_with(|c: char| c.is_ascii_hexdigit())
                    && line.contains(iso_filename)
                {
                    // GNU format: "<hash>  filename"
                    if let Some(hash) = line.split_whitespace().next() {
                        if hash.len() == 64 {
                            return Ok(format!("sha256:{}", hash));
                        }
                    }
                }
            }
        }
    }

    bail!("Could not resolve AlmaLinux ISO checksum from official sources");
}

async fn resolve_xoa_hl_rpm_url(client: &reqwest::Client) -> Result<String> {
    // releases/latest may be one of this agent's xoa-image-* releases, which
    // carries no RPM — scan the list for the newest release that has one.
    let releases = fetch_releases(client, "xoa-hl", 30)
        .await
        .context("Failed to fetch xoa-hl releases")?;

    releases
        .into_iter()
        .flat_map(|r| r.assets)
        .find(|a| a.name.ends_with(".rpm"))
        .map(|a| a.browser_download_url)
        .ok_or_else(|| anyhow::anyhow!("No RPM asset found in recent xoa-hl releases"))
}

// ── PHASE 6: Generate build files ─────────────────────────────────────────────

async fn generate_build_files(build_dir: &Path, config: &BuildConfig) -> Result<()> {
    info!("Creating build directory: {}", build_dir.display());
    async_fs::create_dir_all(build_dir.join("patches")).await?;
    async_fs::create_dir_all(build_dir.join("scripts")).await?;
    async_fs::create_dir_all(build_dir.join("systemd")).await?;

    info!("Generating inst.ks...");
    async_fs::write(build_dir.join("inst.ks"), generate_kickstart(config)).await?;

    info!("Generating almalinux-build.json...");
    async_fs::write(
        build_dir.join("almalinux-build.json"),
        generate_packer_template(config),
    )
    .await?;

    // Copy helper scripts from the synced repo (non-fatal if missing)
    let scripts = [
        "scripts/xoa-first-boot.sh",
        "scripts/xoa-credentials.sh",
        "systemd/xoa-first-boot.service",
        "systemd/xoa-credentials.service",
    ];
    let repo = Path::new(REPO_DIR);
    for script in &scripts {
        let src = repo.join(script);
        let dst = build_dir.join(script);
        if src.exists() {
            async_fs::create_dir_all(dst.parent().unwrap()).await?;
            async_fs::copy(&src, &dst)
                .await
                .with_context(|| format!("Failed to copy {}", script))?;
            info!("Copied {}", script);
        } else {
            warn!("Script not found in repo, skipping: {}", script);
        }
    }

    info!("Build files generated");
    Ok(())
}

fn generate_kickstart(config: &BuildConfig) -> String {
    format!(
        r#"# AlmaLinux {ver} Minimal Kickstart — XOA HomeLab Edition
lang en_US.UTF-8
keyboard us
network --onboot yes --device eth0 --bootproto dhcp
rootpw --plaintext {rootpw}
timezone Asia/Tokyo --utc
selinux --disabled
firewall --disabled
bootloader --location=mbr --boot-drive=xvda
zerombr
clearpart --all --initlabel
part /boot --fstype="ext4" --size=1024
part / --fstype="ext4" --size=1 --grow
reboot

%packages --excludedocs --exclude-weakdeps
@^minimal-environment
@core
chrony
openssh-server
openssh-clients
curl
tar
net-tools
iproute
-linux-firmware
-*-firmware
-lvm2
-dracut-config-rescue
-tuned
-microcode_ctl
-plymouth*
%end

%post --log=/root/ks-post.log
sed -i 's/^SELINUX=.*/SELINUX=disabled/' /etc/selinux/config
systemctl disable network 2>/dev/null || true
systemctl enable NetworkManager --now
systemctl enable sshd --now
sed -i 's/^#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
sed -i 's/^PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
systemctl restart sshd
systemctl enable chronyd --now
echo "tsflags=nodocs" >> /etc/dnf/dnf.conf
echo "install_weak_deps=False" >> /etc/dnf/dnf.conf
dnf install -y epel-release
dnf install -y wget nc vim
groupadd -f xo
useradd -m -g xo -s /bin/bash xo
usermod -aG wheel xo
%end
"#,
        ver = ALMALINUX_VERSION,
        rootpw = config.almalinux_root_password,
    )
}

fn generate_packer_template(config: &BuildConfig) -> String {
    format!(
        r#"{{
  "builders": [{{
    "type": "xenserver-iso",
    "remote_host": "{xcpng_ip}",
    "remote_username": "{xcpng_user}",
    "remote_password": "{xcpng_pass}",
    "iso_url": "{iso_url}",
    "iso_checksum": "{iso_chk}",
    "sr_name": "{sr_name}",
    "vm_name": "{vm_name}",
    "vm_description": "XOA HomeLab Edition — AlmaLinux {ver}",
    "disk_size": {disk_mb},
    "vm_memory": {mem_mb},
    "http_directory": ".",
    "network_names": ["{net}"],
    "boot_command": [
      "<wait5><esc><wait>",
      "linux inst.ks=http://{{{{ .HTTPIP }}}}:{{{{ .HTTPPort }}}}/inst.ks inst.text<enter>"
    ],
    "boot_wait": "5s",
    "ssh_username": "root",
    "ssh_password": "{rootpw}",
    "ssh_timeout": "30m",
    "format": "xva_compressed",
    "keep_vm": "always",
    "skip_set_template": "true",
    "output_directory": "{output_dir}"
  }}],
  "provisioners": [
    {{"type":"shell","inline":["dnf update -y"]}},
    {{"type":"shell","inline":[
      "wget -q {xe_xenstore_url} -O /tmp/xe-guest-utilities-xenstore.rpm",
      "wget -q {xe_url} -O /tmp/xe-guest-utilities.rpm",
      "dnf install -y /tmp/xe-guest-utilities-xenstore.rpm /tmp/xe-guest-utilities.rpm",
      "rm -f /tmp/xe-guest-utilities*.rpm"
    ]}},
    {{"type":"shell","inline":[
      "curl -fsSL https://rpm.nodesource.com/setup_24.x | bash -",
      "dnf install -y nodejs"
    ]}},
    {{"type":"shell","inline":[
      "curl -fsSL '{rpm_url}' -o /tmp/xoa-hl.rpm",
      "dnf install -y /tmp/xoa-hl.rpm",
      "rm -f /tmp/xoa-hl.rpm"
    ]}},
    {{"type":"file","source":"scripts/xoa-first-boot.sh","destination":"/root/xoa-first-boot.sh"}},
    {{"type":"file","source":"scripts/xoa-credentials.sh","destination":"/root/xoa-credentials.sh"}},
    {{"type":"file","source":"systemd/xoa-first-boot.service","destination":"/etc/systemd/system/xoa-first-boot.service"}},
    {{"type":"file","source":"systemd/xoa-credentials.service","destination":"/etc/systemd/system/xoa-credentials.service"}},
    {{"type":"shell","inline":[
      "chmod +x /root/xoa-first-boot.sh /root/xoa-credentials.sh",
      "systemctl daemon-reload",
      "systemctl enable xoa-first-boot.service xoa-credentials.service"
    ]}},
    {{"type":"shell","inline":[
      "dnf remove -y firewalld firewalld-filesystem python3-firewall selinux-policy selinux-policy-targeted policycoreutils",
      "dnf remove -y $(dnf repoquery --installonly --latest-limit=-1 -q) || true",
      "dnf remove -y linux-firmware lvm2 2>/dev/null || true",
      "dnf autoremove -y",
      "dnf clean all",
      "rm -rf /var/cache/dnf/ /var/log/*.log /var/log/journal/* /var/lib/dnf/history* /usr/share/doc/* /usr/share/man/* /usr/share/info/* /usr/share/licenses/*",
      "rm -f /boot/*rescue*",
      "echo -n > /etc/machine-id",
      "dd if=/dev/zero of=/ZERO bs=1M status=none || true; rm -f /ZERO; dd if=/dev/zero of=/boot/ZERO bs=1M status=none || true; rm -f /boot/ZERO; sync"
    ]}}
  ]
}}"#,
        xcpng_ip = config.xcpng_ip,
        xcpng_user = config.xcpng_user,
        xcpng_pass = config.xcpng_password,
        iso_url = config.almalinux_iso_url,
        iso_chk = config.almalinux_iso_checksum,
        sr_name = config.sr_name,
        vm_name = config.vm_name,
        disk_mb = config.vm_disk_size_mb,
        mem_mb = config.vm_memory_mb,
        ver = ALMALINUX_VERSION,
        net = config.vm_network_name,
        rootpw = config.almalinux_root_password,
        xe_xenstore_url = config.xe_guest_utilities_xenstore_url,
        xe_url = config.xe_guest_utilities_url,
        rpm_url = config.xoa_hl_rpm_url,
        output_dir = OUTPUT_DIR,
    )
}

// ── PHASE 7: Run Packer ───────────────────────────────────────────────────────

async fn run_packer(build_dir: &Path) -> Result<()> {
    let packer_file = build_dir.join("almalinux-build.json");

    // Validate first — cheap and catches template errors before a long build
    info!("Running packer validate...");
    let validate = AsyncCommand::new("packer")
        .arg("validate")
        .arg(&packer_file)
        .current_dir(build_dir)
        .output()
        .await
        .context("Failed to run packer validate")?;

    if !validate.status.success() {
        bail!(
            "Packer validate failed:\n{}",
            String::from_utf8_lossy(&validate.stderr)
        );
    }
    info!("Packer validate passed");

    info!(
        "Running packer build (timeout: {} min)...",
        PACKER_TIMEOUT.as_secs() / 60
    );
    let start = Instant::now();

    let mut child = AsyncCommand::new("packer")
        .arg("build")
        // FIX #10: was "-on-error=ask" which blocks on stdin forever in automation.
        //          "-on-error=abort" tears down and exits immediately on failure.
        .arg("-on-error=abort")
        .arg(&packer_file)
        .current_dir(build_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn packer build")?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Stream packer output to the agent log without blocking the wait
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => info!("[packer] {}", line.trim_end()),
                Err(e) => {
                    error!("packer stdout read error: {}", e);
                    break;
                }
            }
        }
    });

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => warn!("[packer:err] {}", line.trim_end()),
                Err(e) => {
                    error!("packer stderr read error: {}", e);
                    break;
                }
            }
        }
    });

    let build_result = tokio::time::timeout(PACKER_TIMEOUT, child.wait()).await;

    stdout_task.abort();
    stderr_task.abort();

    match build_result {
        Ok(Ok(exit)) if exit.success() => {
            info!("Packer build finished in {:?}", start.elapsed());
            Ok(())
        }
        Ok(Ok(exit)) => {
            bail!("Packer build failed (exit code: {:?})", exit.code());
        }
        Ok(Err(e)) => {
            bail!("Packer process error: {}", e);
        }
        Err(_timeout) => {
            // FIX #10 (continued): SIGTERM first, wait grace period, then SIGKILL.
            warn!(
                "Packer timed out after {:?}, sending SIGTERM...",
                PACKER_TIMEOUT
            );

            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // Safety: kill(2) is async-signal-safe; pid is valid.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            }

            match tokio::time::timeout(PACKER_SIGTERM_GRACE, child.wait()).await {
                Ok(_) => info!("Packer exited after SIGTERM"),
                Err(_) => {
                    warn!("Packer did not exit after SIGTERM, sending SIGKILL");
                    let _ = child.kill().await;
                }
            }

            bail!("Packer build timed out after {:?}", PACKER_TIMEOUT);
        }
    }
}

// ── PHASE 8: Locate XVA ───────────────────────────────────────────────────────

async fn locate_xva() -> Result<PathBuf> {
    let output_dir = Path::new(OUTPUT_DIR);

    if !output_dir.exists() {
        bail!("Output directory {} does not exist", output_dir.display());
    }

    // Step 1: collect candidate paths
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut dir = async_fs::read_dir(output_dir)
        .await
        .with_context(|| format!("Cannot read output dir {}", output_dir.display()))?;

    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        let name = path.to_string_lossy();
        if name.ends_with(".xva") || name.ends_with(".xva.gz") {
            candidates.push(path);
        }
    }

    if candidates.is_empty() {
        bail!("No .xva or .xva.gz file found in {}", output_dir.display());
    }

    // FIX #5: was calling async_fs::metadata() inside a sync sort_by closure.
    //         Collect mtimes first (async), then sort synchronously on the results.
    let mut with_mtime: Vec<(PathBuf, Option<std::time::SystemTime>)> = Vec::new();
    for path in candidates {
        let mtime = async_fs::metadata(&path)
            .await
            .ok()
            .and_then(|m| m.modified().ok());
        with_mtime.push((path, mtime));
    }

    // Newest first
    with_mtime.sort_by(|(_, a), (_, b)| b.cmp(a));

    let (path, _) = with_mtime.into_iter().next().unwrap();
    info!("Selected XVA: {}", path.display());
    Ok(path)
}

// ── PHASE 9: Create release and upload ────────────────────────────────────────

fn generate_image_tag(head_sha: &str) -> String {
    let date = Utc::now().format("%Y%m%d");
    let short = &head_sha[..7.min(head_sha.len())];
    format!("{}{}-{}", IMAGE_TAG_PREFIX, date, short)
}

/// Does this release carry the published VM image for the given commit?
/// The tag encodes the short SHA (see generate_image_tag) and the XVA asset
/// must be present — a release whose upload failed doesn't count.
fn is_image_release_for(release: &ReleaseInfo, short_sha: &str) -> bool {
    release.tag_name.starts_with(IMAGE_TAG_PREFIX)
        && release.tag_name.ends_with(&format!("-{}", short_sha))
        && release
            .assets
            .iter()
            .any(|a| a.name.ends_with(".xva") || a.name.ends_with(".xva.gz"))
}

/// Create a GitHub Release, or reuse the existing one (idempotent).
/// Returns `(upload_url, html_url)`.
///
/// The tag is created by the GitHub API using `target_sha` as `target_commitish`.
async fn create_github_release(
    client: &reqwest::Client,
    tag: &str,
    name: &str,
    target_sha: &str,
) -> Result<(String, String)> {
    #[derive(serde::Deserialize)]
    struct ReleaseResp {
        upload_url: String,
        html_url: String,
    }

    // Check whether this exact tag already has a release (retry-safe)
    let check_url = format!(
        "https://api.github.com/repos/{}/releases/tags/{}",
        XOA_HL_REPO, tag
    );
    if let Ok(res) = client.get(&check_url).send().await {
        if res.status().is_success() {
            if let Ok(r) = res.json::<ReleaseResp>().await {
                info!("Release {} already exists, reusing: {}", tag, r.html_url);
                return Ok((
                    r.upload_url.trim_end_matches("{?name,label}").to_string(),
                    r.html_url,
                ));
            }
        }
    }

    // Create a new release; the API creates the tag at target_sha automatically
    let create_url = format!(
        "https://api.github.com/repos/{}/releases",
        XOA_HL_REPO
    );
    let payload = serde_json::json!({
        "tag_name":          tag,
        "target_commitish":  target_sha,
        "name":              name,
        "body":              format!("XOA HomeLab Edition VM image\nSource commit: {}", target_sha),
        "draft":             false,
        "prerelease":        false,
    });

    let res = client
        .post(&create_url)
        .json(&payload)
        .send()
        .await
        .context("Failed to POST GitHub Release")?;

    if !res.status().is_success() {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("GitHub Release creation failed ({}): {}", code, body);
    }

    let release: ReleaseResp = res
        .json()
        .await
        .context("Failed to parse GitHub Release response")?;

    info!("Created GitHub Release: {}", release.html_url);
    Ok((
        release.upload_url.trim_end_matches("{?name,label}").to_string(),
        release.html_url,
    ))
}

/// Stream-upload `xva_path` to an existing GitHub Release upload URL.
async fn upload_asset(
    client: &reqwest::Client,
    upload_url: &str,
    xva_path: &Path,
) -> Result<()> {
    let file_size = async_fs::metadata(xva_path)
        .await
        .with_context(|| format!("Cannot stat {}", xva_path.display()))?
        .len();

    let file_name = xva_path
        .file_name()
        .context("XVA path has no filename")?
        .to_string_lossy()
        .to_string();

    info!(
        "Uploading {} ({:.2} GB)...",
        file_name,
        file_size as f64 / (1024.0_f64.powi(3))
    );

    let file = async_fs::File::open(xva_path)
        .await
        .with_context(|| format!("Cannot open {}", xva_path.display()))?;

    let stream = tokio_util::io::ReaderStream::new(file);

    let res = client
        .post(&format!("{}?name={}", upload_url, file_name))
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", file_size)
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .context("Upload request failed")?;

    if !res.status().is_success() {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("GitHub asset upload failed ({}): {}", code, body);
    }

    info!("XVA uploaded successfully");
    Ok(())
}

// FIX #17: `type Result<T> = anyhow::Result<T>` that was at line 1002 has been
// removed entirely. `use anyhow::Result` at the top of the file already brings
// anyhow::Result into scope — the alias was redundant and confusing.

#[cfg(test)]
mod tests {
    use super::*;
    use shared::ReleaseAsset;

    fn release(tag: &str, asset_names: &[&str]) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: tag.to_string(),
            html_url: format!("https://github.com/Vagrantin/xoa-hl/releases/tag/{}", tag),
            assets: asset_names
                .iter()
                .map(|n| ReleaseAsset {
                    name: n.to_string(),
                    browser_download_url: format!("https://example.com/{}", n),
                })
                .collect(),
        }
    }

    #[test]
    fn image_release_requires_prefix_sha_and_xva_asset() {
        let sha = "cb65556";
        // The real thing: image tag for this SHA with an XVA asset
        assert!(is_image_release_for(&release("xoa-image-20260713-cb65556", &["xoa.xva.gz"]), sha));
        assert!(is_image_release_for(&release("xoa-image-20260713-cb65556", &["xoa.xva"]), sha));
        // RPM release at the same SHA is NOT an image
        assert!(!is_image_release_for(
            &release("v5.113.2_e281c536", &["xoa-hl-5.113.2.el9.noarch.rpm"]),
            sha
        ));
        // Image release for a different commit
        assert!(!is_image_release_for(&release("xoa-image-20260701-090ce7e", &["xoa.xva.gz"]), sha));
        // Image release whose XVA upload failed (no asset) doesn't count
        assert!(!is_image_release_for(&release("xoa-image-20260713-cb65556", &[]), sha));
        assert!(!is_image_release_for(&release("xoa-image-20260713-cb65556", &["notes.txt"]), sha));
    }

    #[test]
    fn generated_image_tag_matches_its_own_release_check() {
        let sha = "cb65556aabbccdd";
        let tag = generate_image_tag(sha);
        assert!(is_image_release_for(&release(&tag, &["xoa.xva.gz"]), &sha[..7]));
    }

    #[test]
    fn build_config_overlays_known_keys() {
        let mut config = BuildConfig::default();
        apply_build_config(
            &mut config,
            r#"
# XCP-ng Hypervisor Connection
XCPNG_IP="192.168.7.42"
XCPNG_USER=admin
SR_NAME='NVMe storage'
VM_NETWORK_NAME="LAN"

VM_DISK_SIZE_MB=20000
VM_MEMORY_MB="4096"
"#,
        )
        .unwrap();
        assert_eq!(config.xcpng_ip, "192.168.7.42");
        assert_eq!(config.xcpng_user, "admin");
        assert_eq!(config.sr_name, "NVMe storage");
        assert_eq!(config.vm_network_name, "LAN");
        assert_eq!(config.vm_disk_size_mb, 20000);
        assert_eq!(config.vm_memory_mb, 4096);
        // Untouched keys keep their defaults
        assert_eq!(config.vm_name, "xoa-almalinux");
    }

    #[test]
    fn build_config_empty_file_keeps_defaults() {
        let mut config = BuildConfig::default();
        apply_build_config(&mut config, "\n# comments only\n").unwrap();
        assert_eq!(config.xcpng_ip, BuildConfig::default().xcpng_ip);
    }

    #[test]
    fn build_config_unknown_key_is_ignored() {
        let mut config = BuildConfig::default();
        apply_build_config(&mut config, "DEBIAN_ISO_URL=\"http://example.com\"\nVM_NAME=xoa\n")
            .unwrap();
        assert_eq!(config.vm_name, "xoa");
    }

    #[test]
    fn shipped_sample_parses_and_matches_defaults() {
        let mut config = BuildConfig::default();
        apply_build_config(&mut config, include_str!("../build.config.sample")).unwrap();
        let d = BuildConfig::default();
        assert_eq!(config.xcpng_ip, d.xcpng_ip);
        assert_eq!(config.xcpng_user, d.xcpng_user);
        assert_eq!(config.sr_name, d.sr_name);
        assert_eq!(config.vm_network_name, d.vm_network_name);
        assert_eq!(config.vm_name, d.vm_name);
        assert_eq!(config.vm_disk_size_mb, d.vm_disk_size_mb);
        assert_eq!(config.vm_memory_mb, d.vm_memory_mb);
        assert_eq!(config.almalinux_iso_url, d.almalinux_iso_url);
        assert_eq!(config.xe_guest_utilities_url, d.xe_guest_utilities_url);
        assert_eq!(config.xe_guest_utilities_xenstore_url, d.xe_guest_utilities_xenstore_url);
    }

    #[test]
    fn build_config_rejects_garbage() {
        let mut config = BuildConfig::default();
        assert!(apply_build_config(&mut config, "not a key value line\n").is_err());
        assert!(apply_build_config(&mut config, "VM_MEMORY_MB=lots\n").is_err());
    }
}
