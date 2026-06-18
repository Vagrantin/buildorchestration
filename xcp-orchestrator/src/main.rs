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
const OLLAMA_URL: &str = "http://localhost:11434/api/generate";
const MODEL_NAME: &str = "qwen3-coder:30b";
const DEFAULT_BRANCH: &str = "main"; 

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

    let trigger_time = Utc::now();

    // PHASE 1: Concurrent Dispatches
    println!("PHASE 1: Triggering Remote Build Pipelines simultaneously via branch '{}'...", DEFAULT_BRANCH);
    let xolite_fut = dispatch_and_locate(&client, "xolite-ce", "build-xolite-ce.yml", trigger_time);
    let xoa_fut = dispatch_and_locate(&client, "xoa-proxy", "xoa-proxy.yml", trigger_time);

    let (res_xolite, res_xoa) = tokio::join!(xolite_fut, xoa_fut);

    let (xolite_id, xolite_url) = res_xolite.expect("Failed to link xolite execution thread");
    state.xolite_id = Some(xolite_id);
    state.xolite_url = xolite_url;
    state.xolite_status = "In Progress".to_string();

    let (xoa_id, xoa_url) = res_xoa.expect("Failed to link xoa proxy execution thread");
    state.xoa_id = Some(xoa_id);
    state.xoa_url = xoa_url;
    state.xoa_status = "In Progress".to_string();

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
        // PHASE 3: Sequential ISO Generation
        println!("PHASE 2 Successful. Moving to PHASE 3: Custom ISO creation...");
        let post_iso_trigger = Utc::now();
        let (iso_id, iso_url) = dispatch_and_locate(&client, "xcp-ng-ce-iso", "build-iso.yml", post_iso_trigger).await?;
        state.iso_id = Some(iso_id);
        state.iso_url = iso_url;
        state.iso_status = "In Progress".to_string();

        loop {
            sleep(Duration::from_secs(30)).await;
            state.iso_status = query_run_conclusion(&client, "xcp-ng-ce-iso", state.iso_id.unwrap()).await?;
            println!("Current ISO State -> {}", state.iso_status);
            if state.iso_status != "In Progress" { break; }
        }
    }

    // Persistence Layer Setup
    write_history_and_render_dashboard(state).await?;
    Ok(())
}

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
        let response = client.get(&check_url).send().await?.json::<GHRunsResponse>().await?;
        
        for run in response.workflow_runs {
            if run.created_at >= (trigger_marker - Duration::from_secs(5)) {
                return Ok((run.id, run.html_url));
            }
        }
    }
    Err(format!("Timeout waiting for run matching dispatch trigger in repo {}", repo).into())
}

async fn query_run_conclusion(client: &reqwest::Client, repo: &str, run_id: u64) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!("https://api.github.com/repos/{}/{}/actions/runs/{}", OWNER, repo, run_id);
    let run = client.get(&url).send().await?.json::<GHRun>().await?;
    if run.status == "completed" {
        Ok(run.conclusion.unwrap_or_else(|| "unknown".to_string()))
    } else {
        Ok("In Progress".to_string())
    }
}

async fn extract_failed_log_context(client: &reqwest::Client, repo: &str, run_id: u64) -> Result<String, Box<dyn std::error::Error>> {
    let jobs_url = format!("https://api.github.com/repos/{}/{}/actions/runs/{}/jobs", OWNER, repo, run_id);
    let res = client.get(&jobs_url).send().await?.json::<GHJobsResponse>().await?;
    
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

fn get_badge_class(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "failure" => "failure",
        "In Progress" => "progress",
        _ => "progress"
    }
}
