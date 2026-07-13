//! Orchestrator - Non-blocking Status Aggregation and Report Engine

use chrono::{DateTime, Utc};
use shared::{
    AgentStatus, PipelineStatus, WorkflowStatus,
    TARGET_REPORT_DIR, HISTORY_FILE,
    storage::{write_atomic_json, load_json_with_default},
    OrchestratorError,
};
use tokio::fs;
use tracing::info;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct RunHistoryItem {
    timestamp: DateTime<Utc>,
    xolite_status: String,
    xoa_proxy_status: String,
    iso_status: String,
    xolite_url: String,
    xoa_proxy_url: String,
    iso_url: String,
    #[serde(default)]
    xoa_hl_status: String,
    #[serde(default)]
    xoa_image_status: String,
    #[serde(default)]
    xoa_hl_url: String,
    #[serde(default)]
    xoa_image_url: String,
    llm_hint: Option<String>,
}

impl Default for RunHistoryItem {
    fn default() -> Self {
        Self {
            timestamp: Utc::now(),
            xolite_status: "Skipped".to_string(),
            xoa_proxy_status: "Skipped".to_string(),
            iso_status: "Skipped".to_string(),
            xolite_url: "#".to_string(),
            xoa_proxy_url: "#".to_string(),
            iso_url: "#".to_string(),
            xoa_hl_status: "Skipped".to_string(),
            xoa_image_status: "Skipped".to_string(),
            xoa_hl_url: "#".to_string(),
            xoa_image_url: "#".to_string(),
            llm_hint: None,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), OrchestratorError> {
    tracing_subscriber::fmt()
        .with_env_filter("orchestrator=info")
        .init();

    info!("Starting Async Orchestrator loop...");

    // 1. Asynchronously read status payloads from individual runtime agent locations
    let iso_path = "/var/lib/xcp-hl-orchestrator/xcp-iso-agent.status.json";
    let xoa_path = "/var/lib/xcp-hl-orchestrator/xoa-vm-agent.status.json";

    let iso_agent_status: Option<AgentStatus> = load_json_with_default(iso_path).await.ok();
    let xoa_vm_agent_status: Option<AgentStatus> = load_json_with_default(xoa_path).await.ok();

    // 2. Synthesize pipeline views
    let mut pipeline_status = PipelineStatus::new();
    pipeline_status.iso_agent = iso_agent_status.clone();
    pipeline_status.xoa_vm_agent = xoa_vm_agent_status.clone();
    pipeline_status.update_overall();

    let mut llm_hint: Option<String> = None;

    if pipeline_status.overall == WorkflowStatus::Failure {
        info!("Failures located. Dispatching diagnostic tracking operations...");
        // Log gathering and evaluation logic goes here...
        llm_hint = Some("Verify network interfaces and upstream tag validity fields.".to_string());
    }

    // 3. Formulate structural history tracking context
    let mut history_item = RunHistoryItem::default();
    history_item.timestamp = Utc::now();
    history_item.llm_hint = llm_hint;

    if let Some(ref status) = iso_agent_status {
        history_item.xolite_status = status.status.to_string();
        history_item.xoa_proxy_status = status.status.to_string();
        history_item.iso_status = status.status.to_string();
        history_item.xolite_url = status.url.clone();
        history_item.xoa_proxy_url = status.url.clone();
        history_item.iso_url = status.url.clone();
    }

    if let Some(ref status) = xoa_vm_agent_status {
        history_item.xoa_hl_status = status.status.to_string();
        history_item.xoa_image_status = status.status.to_string();
        history_item.xoa_hl_url = status.url.clone();
        history_item.xoa_image_url = status.url.clone();
    }

    // 4. Update ledger using safe, non-blocking asynchronous calls
    update_history_async(history_item).await?;
    render_dashboard_async().await?;

    Ok(())
}

async fn update_history_async(item: RunHistoryItem) -> Result<(), OrchestratorError> {
    let mut history: Vec<RunHistoryItem> = load_json_with_default(HISTORY_FILE).await.unwrap_or_default();
    
    history.insert(0, item);
    if history.len() > 15 {
        history.truncate(15);
    }

    write_atomic_json(HISTORY_FILE, &history).await?;
    Ok(())
}

async fn render_dashboard_async() -> Result<(), OrchestratorError> {
    fs::create_dir_all(TARGET_REPORT_DIR).await?;
    let history: Vec<RunHistoryItem> = load_json_with_default(HISTORY_FILE).await.unwrap_or_default();

    let mut html = String::from(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>XCP-ng Pipeline Status</title></head><body>"#);
    html.push_str("<h1>XCP-ng Active Agent Status Grid</h1>");

    for item in history {
        html.push_str(&format!(r#"<div><h3>Run: {}</h3>"#, item.timestamp.to_rfc3339()));
        html.push_str(&format!(r#"<p>ISO Target State: {}</p></div>"#, item.iso_status));
    }
    html.push_str(r#"</body></html>"#);

    let output_path = format!("{}/build_report.html", TARGET_REPORT_DIR);
    fs::write(&output_path, html).await?;
    
    Ok(())
}
