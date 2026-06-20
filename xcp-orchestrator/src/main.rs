use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::time::sleep;

const OWNER: &str = "Vagrantin";
const TARGET_REPORT_DIR: &str = "/var/www/html/orchestrator";
const STATE_FILE: &str = "/var/lib/xcp-hl-orchestrator/history.json";
const VERSION_STATE_FILE: &str = "/var/lib/xcp-hl-orchestrator/version_state.json";
const OLLAMA_URL: &str = "http://localhost:11434/api/generate";
const MODEL_NAME: &str = "qwen3-coder:30b";
const DEFAULT_BRANCH: &str = "main";
const DOCS_REPO: &str = "xcp-hl";
const RELEASES_DATA_PATH: &str = "docs/_data/releases.yml";

/// Target XCP-ng base release this edition is built against (e.g. "8.3").
/// build-iso.yml derives its docker image tag and updates.xcp-ng.org repo
/// paths from this value via the git tag, so it can't be date-based — bump
/// this constant manually when you decide to target a new XCP-ng release.
const XCPNG_TARGET_VERSION: &str = "8.3";

// ── History / dashboard types (unchanged) ──────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct RunHistoryItem {
    timestamp: DateTime<Utc>,
    xolite_status: String,
    xoa_proxy_status: String,
    iso_status: String,
    xolite_url: String,
    xoa_proxy_url: String,
    iso_url: String,
    llm_hint: Option<String>,
}

#[derive(Deserialize, Debug)]
struct GHRun {
    id: u64,
    html_url: String,
    status: String,
    conclusion: Option<String>,
    created_at: DateTime<Utc>,
    head_branch: Option<String>,
}

#[derive(Deserialize, Debug)]
struct GHRunsResponse {
    workflow_runs: Vec<GHRun>,
}

#[derive(Deserialize, Debug)]
struct GHJob {
    id: u64,
    conclusion: Option<String>,
}

#[derive(Deserialize, Debug)]
struct GHJobsResponse {
    jobs: Vec<GHJob>,
}

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

struct PipelineState {
    xolite_id: Option<u64>,
    xolite_status: String,
    xolite_url: String,
    xoa_id: Option<u64>,
    xoa_status: String,
    xoa_url: String,
    iso_id: Option<u64>,
    iso_status: String,
    iso_url: String,
    llm_hint: Option<String>,
}

// ── Version state: persisted source of truth for tag derivation ───────────
// Separate from history.json (which is run telemetry/dashboard data).

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct ComponentVersionState {
    /// Upstream version we last built against (e.g. "0.34.2"). Empty if never built.
    upstream_version: String,
    /// Our patch-revision counter against that upstream_version (the "ce" suffix).
    ce_counter: u32,
    /// Full tag we last pushed (e.g. "v0.34.2-ce3"). Empty if never built.
    last_tag: String,
    /// SHA of our own repo's main branch at the time of last_tag, used to detect
    /// "we changed our patches/spec but upstream didn't move" without re-diffing.
    last_built_sha: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct IsoVersionState {
    /// XCP-ng base version this edition currently targets (e.g. "8.3").
    xcpng_version: String,
    /// Community build counter for that xcpng_version (the "ce" suffix).
    ce_counter: u32,
    /// Full tag we last pushed (e.g. "v8.3-ce4"). Empty if never built.
    last_tag: String,
    /// Component tags baked into the last ISO build — used to skip rebuilding
    /// when neither component has advanced since.
    last_xolite_tag: String,
    last_xoa_proxy_tag: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct VersionState {
    xolite_ce: ComponentVersionState,
    iso: IsoVersionState,
    // xoa_hl: ComponentVersionState,  // added when the XVA flow is designed
}

fn load_version_state() -> VersionState {
    if Path::new(VERSION_STATE_FILE).exists() {
        serde_json::from_str(&fs::read_to_string(VERSION_STATE_FILE).unwrap_or_default())
            .unwrap_or_default()
    } else {
        VersionState::default()
    }
}

fn save_version_state(state: &VersionState) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(Path::new(VERSION_STATE_FILE).parent().unwrap())?;
    fs::write(VERSION_STATE_FILE, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting XCP-ng Agentic Automation Pipeline...");

    // 1. Recover Secure Token from Systemd Credentials Sandbox
    let token = match env::var("CREDENTIALS_DIRECTORY") {
        Ok(dir) => {
            let p = PathBuf::from(dir).join("GH_TOKEN");
            fs::read_to_string(p)?.trim().to_string()
        }
        Err(_) => env::var("GH_TOKEN").expect("GH_TOKEN variable must be bound contextually"),
    };

    let client_builder = reqwest::Client::builder();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Authorization", format!("Bearer {}", token).parse()?);
    headers.insert("User-Agent", "XCP-Orchestrator-Rust-Agent".parse()?);
    headers.insert("Accept", "application/vnd.github+json".parse()?);
    let client = client_builder.default_headers(headers).build()?;

    let mut state = PipelineState {
        xolite_id: None, xolite_status: "Skipped".to_string(), xolite_url: "#".to_string(),
        xoa_id: None, xoa_status: "Skipped".to_string(), xoa_url: "#".to_string(),
        iso_id: None, iso_status: "Skipped".to_string(), iso_url: "#".to_string(),
        llm_hint: None,
    };

    let mut version_state = load_version_state();
    let trigger_time = Utc::now();

    // PHASE 1: Decide xolite-ce's tag (if any) and dispatch xoa-proxy, concurrently.
    println!("PHASE 1: Evaluating component changes and triggering builds...");

    let xolite_branch = async {
        let head_sha = fetch_repo_head_sha(&client, "xolite-ce").await?;
        let decision = decide_xolite_bump(&client, &version_state.xolite_ce).await?;
        Ok::<(String, BumpDecision), Box<dyn std::error::Error>>((head_sha, decision))
    };
    let xoa_branch = dispatch_and_locate(&client, "xoa-proxy", "xoa-proxy.yml", trigger_time);

    let (xolite_res, xoa_res) = tokio::join!(xolite_branch, xoa_branch);

    let (xoa_id, xoa_url) = xoa_res.expect("Failed to link xoa proxy execution thread");
    state.xoa_id = Some(xoa_id);
    state.xoa_url = xoa_url;
    state.xoa_status = "In Progress".to_string();

    let xolite_head_sha: String;
    let mut xolite_tag: Option<String> = None;

    match xolite_res {
        Ok((head_sha, BumpDecision::NoChange)) => {
            println!("▶ xolite-ce: no upstream or local change detected, skipping build.");
            xolite_head_sha = head_sha;
        }
        Ok((head_sha, BumpDecision::UpstreamBump { upstream_version })) => {
            let tag = format!("v{}-ce1", upstream_version);
            version_state.xolite_ce.upstream_version = upstream_version;
            version_state.xolite_ce.ce_counter = 1;
            create_and_push_tag(&client, "xolite-ce", &tag, &head_sha).await?;
            let (id, url) = locate_tag_triggered_run(&client, "xolite-ce", &tag, trigger_time).await?;
            state.xolite_id = Some(id);
            state.xolite_url = url;
            state.xolite_status = "In Progress".to_string();
            xolite_head_sha = head_sha;
            xolite_tag = Some(tag);
        }
        Ok((head_sha, BumpDecision::PatchBump { upstream_version, next_counter })) => {
            let tag = format!("v{}-ce{}", upstream_version, next_counter);
            version_state.xolite_ce.ce_counter = next_counter;
            create_and_push_tag(&client, "xolite-ce", &tag, &head_sha).await?;
            let (id, url) = locate_tag_triggered_run(&client, "xolite-ce", &tag, trigger_time).await?;
            state.xolite_id = Some(id);
            state.xolite_url = url;
            state.xolite_status = "In Progress".to_string();
            xolite_head_sha = head_sha;
            xolite_tag = Some(tag);
        }
        Err(e) => {
            println!("⚠ xolite-ce bump detection failed: {}. Skipping build this run.", e);
            xolite_head_sha = fetch_repo_head_sha(&client, "xolite-ce")
                .await
                .unwrap_or_default();
        }
    }

    // PHASE 2: Parallel Monitoring loop
    println!("PHASE 2: Awaiting parallel compilation completions...");
    loop {
        sleep(Duration::from_secs(20)).await;

        if state.xolite_status == "In Progress" {
            state.xolite_status = query_run_conclusion(&client, "xolite-ce", state.xolite_id.unwrap()).await?;
        }
        if state.xoa_status == "In Progress" {
            state.xoa_status = query_run_conclusion(&client, "xoa-proxy", state.xoa_id.unwrap()).await?;
        }

        println!("Current Sync State -> Xolite-ce: {} | Xoa-Proxy: {}", state.xolite_status, state.xoa_status);

        if state.xolite_status != "In Progress" && state.xoa_status != "In Progress" {
            break;
        }
    }

    // Evaluate Phase 2 Failures and trigger diagnostic models
    if state.xolite_status == "failure" || state.xoa_status == "failure" {
        println!("Execution failure intercepted. Running diagnostics...");

        let target_log = if state.xolite_status == "failure" {
            extract_failed_log_context(&client, "xolite-ce", state.xolite_id.unwrap()).await?
        } else {
            extract_failed_log_context(&client, "xoa-proxy", state.xoa_id.unwrap()).await?
        };

        state.llm_hint = Some(evaluate_log_via_ollama(&client, &target_log).await?);
        state.iso_status = "Aborted".to_string();
    } else {
        // Persist xolite-ce version state now that its build (if any) succeeded.
        if state.xolite_status == "success" {
            if let Some(tag) = xolite_tag {
                version_state.xolite_ce.last_tag = tag;
                version_state.xolite_ce.last_built_sha = xolite_head_sha;
                save_version_state(&version_state)?;
            }
        }

        // PHASE 3: ISO generation — only if xolite-ce or xoa-proxy actually advanced,
        // or the targeted XCP-ng base version changed (Axis 3: ISO always uses latest tags).
        let xolite_version = fetch_latest_repo_tag(&client, "xolite-ce").await.unwrap_or_default();
        let xoa_proxy_version = fetch_latest_repo_tag(&client, "xoa-proxy").await.unwrap_or_default();

        let needs_iso_build = version_state.iso.xcpng_version != XCPNG_TARGET_VERSION
            || version_state.iso.last_xolite_tag != xolite_version
            || version_state.iso.last_xoa_proxy_tag != xoa_proxy_version;

        if !needs_iso_build {
            println!("▶ xcp-ng-ce-iso: no component change since last ISO build, skipping.");
            state.iso_status = "Skipped".to_string();
        } else {
            println!("PHASE 2 Successful. Moving to PHASE 3: Custom ISO creation...");

            let next_counter = if version_state.iso.xcpng_version == XCPNG_TARGET_VERSION {
                version_state.iso.ce_counter + 1
            } else {
                1
            };
            let iso_tag = format!("v{}-ce{}", XCPNG_TARGET_VERSION, next_counter);

            let iso_head_sha = fetch_repo_head_sha(&client, "xcp-ng-ce-iso").await?;
            create_and_push_tag(&client, "xcp-ng-ce-iso", &iso_tag, &iso_head_sha).await?;

            let post_iso_trigger = Utc::now();
            let (iso_id, iso_url) =
                locate_tag_triggered_run(&client, "xcp-ng-ce-iso", &iso_tag, post_iso_trigger).await?;
            state.iso_id = Some(iso_id);
            state.iso_url = iso_url;
            state.iso_status = "In Progress".to_string();

            loop {
                sleep(Duration::from_secs(30)).await;
                state.iso_status = query_run_conclusion(&client, "xcp-ng-ce-iso", state.iso_id.unwrap()).await?;
                println!("Current ISO State -> {}", state.iso_status);
                if state.iso_status != "In Progress" { break; }
            }

            if state.iso_status == "success" {
                version_state.iso.xcpng_version = XCPNG_TARGET_VERSION.to_string();
                version_state.iso.ce_counter = next_counter;
                version_state.iso.last_tag = iso_tag.clone();
                version_state.iso.last_xolite_tag = xolite_version.clone();
                version_state.iso.last_xoa_proxy_tag = xoa_proxy_version.clone();
                save_version_state(&version_state)?;

                if let Err(e) = append_release_matrix_entry(
                    &client,
                    &iso_tag,
                    &xolite_version,
                    &version_state.xolite_ce.upstream_version,
                    &xoa_proxy_version,
                )
                .await
                {
                    println!("⚠ Failed to update release matrix: {}", e);
                }
            }
        }
    }

    // Persistence Layer Setup
    write_history_and_render_dashboard(state).await?;
    Ok(())
}

// ── Dispatch / run-location (workflow_dispatch path — used by xoa-proxy) ──

async fn dispatch_and_locate(client: &reqwest::Client, repo: &str, workflow: &str, trigger_marker: DateTime<Utc>) -> Result<(u64, String), Box<dyn std::error::Error>> {
    let url = format!("https://api.github.com/repos/{}/{}/actions/workflows/{}/dispatches", OWNER, repo, workflow);
    let payload = serde_json::json!({ "ref": DEFAULT_BRANCH });

    let res = client.post(&url).json(&payload).send().await?;
    if res.status() != reqwest::StatusCode::NO_CONTENT {
        return Err(format!("Dispatch rejection for repo: {}. Code: {}", repo, res.status()).into());
    }

    // Mitigate 204 Race Condition: Poll runs array until the newest target registers
    let check_url = format!("https://api.github.com/repos/{}/{}/actions/runs?event=workflow_dispatch&per_page=5", OWNER, repo);
    let timeout_limit = Instant::now() + Duration::from_secs(45);

    while Instant::now() < timeout_limit {
        sleep(Duration::from_secs(4)).await;
        let response: GHRunsResponse =
            parse_github_response(client.get(&check_url).send().await?, "dispatch_and_locate runs list").await?;

        for run in response.workflow_runs {
            if run.created_at >= (trigger_marker - Duration::from_secs(5)) {
                return Ok((run.id, run.html_url));
            }
        }
    }
    Err(format!("Timeout waiting for run matching dispatch trigger in repo {}", repo).into())
}

// ── Tag creation + locating the run it triggers (tag-push path) ───────────

async fn create_and_push_tag(
    client: &reqwest::Client,
    repo: &str,
    tag: &str,
    sha: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("https://api.github.com/repos/{}/{}/git/refs", OWNER, repo);
    let payload = serde_json::json!({
        "ref": format!("refs/tags/{}", tag),
        "sha": sha,
    });
    let res = client.post(&url).json(&payload).send().await?;
    if !res.status().is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(format!("Failed to create tag {} on {}: {}", tag, repo, body).into());
    }
    println!("▶ Pushed tag {} on {} (sha {})", tag, repo, &sha[..7.min(sha.len())]);
    Ok(())
}

/// Like dispatch_and_locate, but for workflows triggered by a tag push rather
/// than workflow_dispatch — polls runs filtered to event=push.
async fn locate_tag_triggered_run(
    client: &reqwest::Client,
    repo: &str,
    tag: &str,
    trigger_marker: DateTime<Utc>,
) -> Result<(u64, String), Box<dyn std::error::Error>> {
    let check_url = format!(
        "https://api.github.com/repos/{}/{}/actions/runs?event=push&per_page=5",
        OWNER, repo
    );
    // Tag-push runs are triggered by GitHub's async webhook/event pipeline
    // rather than the synchronous workflow_dispatch API, and have historically
    // shown longer, more variable registration latency. 45s (the window used
    // for workflow_dispatch, where it's proven reliable) wasn't enough here —
    // widened to 3 minutes, polling less aggressively since we have more
    // budget. head_branch matching added now that the window is wider, so a
    // longer wait doesn't risk picking up an unrelated concurrent run.
    let timeout_limit = Instant::now() + Duration::from_secs(180);

    while Instant::now() < timeout_limit {
        sleep(Duration::from_secs(6)).await;
        let response: GHRunsResponse = parse_github_response(
            client.get(&check_url).send().await?,
            &format!("locate_tag_triggered_run for {} on {}", tag, repo),
        )
        .await?;

        for run in response.workflow_runs {
            let branch_matches = run.head_branch.as_deref() == Some(tag);
            let time_matches = run.created_at >= (trigger_marker - Duration::from_secs(5));
            if branch_matches || (run.head_branch.is_none() && time_matches) {
                return Ok((run.id, run.html_url));
            }
        }
    }
    Err(format!(
        "Timeout waiting for tag-push run matching {} in repo {}",
        tag, repo
    )
    .into())
}

async fn query_run_conclusion(client: &reqwest::Client, repo: &str, run_id: u64) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!("https://api.github.com/repos/{}/{}/actions/runs/{}", OWNER, repo, run_id);
    let run: GHRun = parse_github_response(client.get(&url).send().await?, "query_run_conclusion").await?;
    if run.status == "completed" {
        Ok(run.conclusion.unwrap_or_else(|| "unknown".to_string()))
    } else {
        Ok("In Progress".to_string())
    }
}

async fn extract_failed_log_context(client: &reqwest::Client, repo: &str, run_id: u64) -> Result<String, Box<dyn std::error::Error>> {
    let jobs_url = format!("https://api.github.com/repos/{}/{}/actions/runs/{}/jobs", OWNER, repo, run_id);
    let res: GHJobsResponse =
        parse_github_response(client.get(&jobs_url).send().await?, "extract_failed_log_context jobs list").await?;

    if let Some(failed_job) = res.jobs.iter().find(|j| j.conclusion.as_deref() == Some("failure")) {
        let log_url = format!("https://api.github.com/repos/{}/{}/actions/jobs/{}/logs", OWNER, repo, failed_job.id);
        let log_text = client.get(&log_url).send().await?.text().await?;

        let lines: Vec<&str> = log_text.lines().collect();
        let tail_count = lines.len().min(250);
        let truncated = lines[lines.len() - tail_count..].join("\n");
        return Ok(truncated);
    }
    Ok("Could not resolve failed job log metrics.".to_string())
}

async fn evaluate_log_via_ollama(client: &reqwest::Client, raw_logs: &str) -> Result<String, Box<dyn std::error::Error>> {
    let prompt = format!(
        "You are an expert XCP-ng software integration engineer. Analyze the following failed build logs.\n\
        Pinpoint the exact reason for failure (e.g., missing dependencies, network drops, compilation error).\n\
        Provide highly concise, actionable hints to remediate the setup.\n\n\
        ### BUILD LOG EXCERPT:\n{}\n\n### REMEDIATION HINT:", raw_logs);

    let payload = OllamaRequest { model: MODEL_NAME.to_string(), prompt, stream: false };

    let res = client.post(OLLAMA_URL).json(&payload).timeout(Duration::from_secs(300)).send().await?;
    let json_res = res.json::<OllamaResponse>().await?;
    Ok(json_res.response)
}

// ── Upstream + own-repo change detection (xolite-ce) ───────────────────────

#[derive(Deserialize, Debug)]
struct GHTagRef {
    #[serde(rename = "ref")]
    git_ref: String, // "refs/tags/xo-lite-v0.34.2"
}

/// Mirrors the workflow's "git ls-remote --tags --sort=v:refname ... xo-lite-v*"
/// step, via the GitHub API instead of a clone, against vatesfr/xen-orchestra.
async fn fetch_latest_upstream_xolite_tag(
    client: &reqwest::Client,
) -> Result<String, Box<dyn std::error::Error>> {
    let url = "https://api.github.com/repos/vatesfr/xen-orchestra/git/refs/tags";
    let refs: Vec<GHTagRef> =
        parse_github_response(client.get(url).send().await?, "fetch_latest_upstream_xolite_tag").await?;

    let mut tags: Vec<String> = refs
        .into_iter()
        .filter_map(|r| r.git_ref.strip_prefix("refs/tags/xo-lite-v").map(String::from))
        .collect();

    // Semver-ish sort: split on '.', compare numerically. Good enough for
    // "0.34.2" style tags; falls back to lexicographic if parsing fails.
    tags.sort_by(|a, b| {
        let pa: Vec<u32> = a.split('.').filter_map(|s| s.parse().ok()).collect();
        let pb: Vec<u32> = b.split('.').filter_map(|s| s.parse().ok()).collect();
        pa.cmp(&pb)
    });

    tags.pop().ok_or_else(|| "No xo-lite-v* tags found upstream".into())
}

/// Mirrors the workflow's "jq -r .version package.json" step, via Contents API
/// against the upstream tag (no clone needed).
async fn fetch_upstream_xolite_version(
    client: &reqwest::Client,
    upstream_tag: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!(
        "https://api.github.com/repos/vatesfr/xen-orchestra/contents/@xen-orchestra/lite/package.json?ref=xo-lite-v{}",
        upstream_tag
    );
    #[derive(Deserialize)]
    struct ContentResp {
        content: String,
    }
    // NOTE: previously sent `Accept: application/vnd.github.raw+json` here, which
    // tells GitHub to return the raw file bytes instead of the standard Contents
    // API envelope ({content, sha, ...}) — that's what produced the "missing
    // field `content`" crash, since the response was real package.json text
    // (hence the multi-line parse error), not the expected base64 envelope.
    // The client's default Accept (application/vnd.github+json, set globally
    // in main()) already gives us the envelope shape ContentResp expects.
    let resp: ContentResp = parse_github_response(
        client.get(&url).send().await?,
        &format!("fetch_upstream_xolite_version for tag xo-lite-v{}", upstream_tag),
    )
    .await?;

    // Contents API returns base64 with embedded newlines.
    use base64::{engine::general_purpose, Engine as _};
    let cleaned: String = resp.content.chars().filter(|c| !c.is_whitespace()).collect();
    let decoded = general_purpose::STANDARD.decode(cleaned)?;
    let pkg: serde_json::Value = serde_json::from_slice(&decoded)?;
    Ok(pkg["version"]
        .as_str()
        .ok_or("package.json missing version field")?
        .to_string())
}

/// Latest commit SHA on our own repo's main branch — used to tell "patches/spec
/// changed since we last built" apart from "nothing changed, don't rebuild".
async fn fetch_repo_head_sha(
    client: &reqwest::Client,
    repo: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/commits/{}",
        OWNER, repo, DEFAULT_BRANCH
    );
    #[derive(Deserialize)]
    struct CommitResp {
        sha: String,
    }
    let resp: CommitResp = parse_github_response(
        client.get(&url).send().await?,
        &format!("fetch_repo_head_sha for {}", repo),
    )
    .await?;
    Ok(resp.sha)
}

enum BumpDecision {
    /// Nothing changed upstream or locally — skip the build entirely.
    NoChange,
    /// Upstream moved to a new version — reset counter to 1.
    UpstreamBump { upstream_version: String },
    /// Same upstream version, but our own patches/spec changed — bump counter.
    PatchBump { upstream_version: String, next_counter: u32 },
}

async fn decide_xolite_bump(
    client: &reqwest::Client,
    state: &ComponentVersionState,
) -> Result<BumpDecision, Box<dyn std::error::Error>> {
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

// ── Release matrix update (docs site) ──────────────────────────────────────

#[derive(Deserialize)]
struct GHFileContent {
    content: String,
    sha: String,
}

async fn append_release_matrix_entry(
    client: &reqwest::Client,
    iso_tag: &str,
    xolite_version: &str,
    xolite_upstream: &str,
    xoa_proxy_version: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use base64::{engine::general_purpose, Engine as _};

    let url = format!(
        "https://api.github.com/repos/{}/{}/contents/{}",
        OWNER, DOCS_REPO, RELEASES_DATA_PATH
    );
    let existing: GHFileContent =
        parse_github_response(client.get(&url).send().await?, "append_release_matrix_entry GET").await?;
    let cleaned: String = existing.content.chars().filter(|c| !c.is_whitespace()).collect();
    let current_yaml = String::from_utf8(general_purpose::STANDARD.decode(cleaned)?)?;

    let new_entry = format!(
        "- iso_version: \"{iso}\"\n  build_date: \"{date}\"\n  components:\n    xolite_ce:\n      version: \"{xv}\"\n      upstream: \"{xu}\"\n      upstream_url: \"https://github.com/vatesfr/xen-orchestra/releases/tag/xo-lite-v{xu}\"\n    xoa_proxy:\n      version: \"{pv}\"\n",
        iso = iso_tag,
        date = Utc::now().format("%Y-%m-%d"),
        xv = xolite_version,
        xu = xolite_upstream,
        pv = xoa_proxy_version,
    );
    let updated_yaml = format!("{}\n{}", new_entry.trim_end(), current_yaml);

    let payload = serde_json::json!({
        "message": format!("docs: record release {} in matrix", iso_tag),
        "content": general_purpose::STANDARD.encode(updated_yaml.as_bytes()),
        "sha": existing.sha,
        "branch": DEFAULT_BRANCH,
    });
    let res = client.put(&url).json(&payload).send().await?;
    if !res.status().is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(format!("Failed to update release matrix: {}", body).into());
    }
    println!("▶ Release matrix updated with {} (push will trigger pages.yml)", iso_tag);
    Ok(())
}

/// Fetch the latest tag on a repo (used to read xoa-proxy's manually-set version
/// for the matrix, and to read xolite-ce's version back after a build/skip).
async fn fetch_latest_repo_tag(
    client: &reqwest::Client,
    repo: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!("https://api.github.com/repos/{}/{}/tags?per_page=1", OWNER, repo);
    #[derive(Deserialize)]
    struct TagEntry {
        name: String,
    }
    let tags: Vec<TagEntry> = parse_github_response(
        client.get(&url).send().await?,
        &format!("fetch_latest_repo_tag for {}", repo),
    )
    .await?;
    tags.into_iter()
        .next()
        .map(|t| t.name)
        .ok_or_else(|| format!("No tags found on {}", repo).into())
}

// ── Dashboard rendering (unchanged) ─────────────────────────────────────────

async fn write_history_and_render_dashboard(current: PipelineState) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(Path::new(STATE_FILE).parent().unwrap())?;
    fs::create_dir_all(TARGET_REPORT_DIR)?;

    let mut history: Vec<RunHistoryItem> = if Path::new(STATE_FILE).exists() {
        serde_json::from_str(&fs::read_to_string(STATE_FILE)?)?
    } else { Vec::new() };

    let current_item = RunHistoryItem {
        timestamp: Utc::now(),
        xolite_status: current.xolite_status,
        xoa_proxy_status: current.xoa_status,
        iso_status: current.iso_status,
        xolite_url: current.xolite_url,
        xoa_proxy_url: current.xoa_url,
        iso_url: current.iso_url,
        llm_hint: current.llm_hint,
    };

    history.insert(0, current_item);
    if history.len() > 15 { history.truncate(15); }
    fs::write(STATE_FILE, serde_json::to_string_pretty(&history)?)?;

    let mut html = String::from(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>XCP-ng Engine Pipeline</title>
    <style>body{font-family:sans-serif;background:#121214;color:#e1e1e6;margin:40px;} h1{color:#4f46e5;}
    .card{background:#1c1c1f;padding:20px;border-radius:8px;margin-bottom:20px;border:1px solid #2d2d34;}
    .badge{padding:4px 8px;border-radius:4px;font-size:12px;font-weight:bold;}
    .success{background:#166534;color:#bbf7d0;} .failure{background:#991b1b;color:#fca5a5;} .progress{background:#854d0e;color:#fef08a;}
    a{color:#6366f1;text-decoration:none;} a:hover{text-decoration:underline;} pre{background:#09090b;padding:15px;border-radius:6px;overflow-x:auto;color:#fda4af;border-left:4px solid #f43f5e;}</style></head><body>
    <h1>XCP-ng Agentic Build Dashboard</h1>"#);

    for (idx, item) in history.iter().enumerate() {
        let title_prefix = if idx == 0 { "Latest Execution Run" } else { "Historical Archive" };
        html.push_str(&format!(r#"<div class="card"><h3>{} ({})</h3>
        <p><b>Xolite-ce:</b> <span class="badge {}">{}</span> | <a href="{}" target="_blank">Logs</a></p>
        <p><b>XOA-Proxy:</b> <span class="badge {}">{}</span> | <a href="{}" target="_blank">Logs</a></p>
        <p><b>ISO Matrix:</b> <span class="badge {}">{}</span> | <a href="{}" target="_blank">Logs</a></p>"#,
        title_prefix, item.timestamp.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S"),
        get_badge_class(&item.xolite_status), item.xolite_status, item.xolite_url,
        get_badge_class(&item.xoa_proxy_status), item.xoa_proxy_status, item.xoa_proxy_url,
        get_badge_class(&item.iso_status), item.iso_status, item.iso_url));

        if let Some(ref hint) = item.llm_hint {
            html.push_str(&format!(r#"<h4>🤖 Qwen-Coder Diagnostic Remediation Analysis:</h4><pre>{}</pre>"#, hint));
        }
        html.push_str("</div>");
    }

    html.push_str("</body></html>");
    fs::write(format!("{}/build_report.html", TARGET_REPORT_DIR), html)?;
    println!("Dashboard successfully rendered to {}/build_report.html", TARGET_REPORT_DIR);
    Ok(())
}

// ── Shared response parsing — checks status before deserializing ──────────
// Every GitHub API call in this file was previously doing
// `.send().await?.json::<T>().await?`, which attempts to deserialize error
// bodies (404/403/etc) as if they were success payloads. This centralizes
// the fix: read the body once, check status, give a useful error either way.
async fn parse_github_response<T: serde::de::DeserializeOwned>(
    res: reqwest::Response,
    context: &str,
) -> Result<T, Box<dyn std::error::Error>> {
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        return Err(format!(
            "GitHub API error ({}) during {}: {}",
            status, context, body
        )
        .into());
    }
    serde_json::from_str(&body).map_err(|e| {
        format!(
            "Failed to parse JSON during {} (status {}): {}\nBody (first 500 chars): {}",
            context,
            status,
            e,
            &body[..body.len().min(500)]
        )
        .into()
    })
}

fn get_badge_class(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "failure" => "failure",
        "In Progress" => "progress",
        _ => "progress"
    }
}
