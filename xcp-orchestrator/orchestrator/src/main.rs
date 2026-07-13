//! Orchestrator - Non-blocking Status Aggregation and Report Engine

use chrono::{DateTime, Utc};
use shared::{
    AgentStatus, PipelineStatus, WorkflowStatus,
    TARGET_REPORT_DIR, HISTORY_FILE,
    storage::{write_atomic_json, load_json_with_default},
    OrchestratorError,
};
use tokio::fs;
use tracing::{error, info};

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

/// Read a component's (status, url) from an agent status, falling back to the
/// agent-level fields so older status files (without `components`) still render.
fn component_or_agent(status: &AgentStatus, name: &str) -> (String, String) {
    match status.component(name) {
        Some(c) => (
            c.status.to_string(),
            if c.url.is_empty() { "#".to_string() } else { c.url.clone() },
        ),
        None => (
            status.status.to_string(),
            if status.url.is_empty() { "#".to_string() } else { status.url.clone() },
        ),
    }
}

fn log_agent_failure(agent_name: &str, status: &AgentStatus) {
    let is_failure = matches!(
        status.status,
        WorkflowStatus::Failure | WorkflowStatus::Timeout | WorkflowStatus::Aborted
    );
    if !is_failure {
        return;
    }
    let logs = if status.url.is_empty() { "n/a" } else { status.url.as_str() };
    error!(
        "{} {} in phase '{}': {} — logs: {}",
        agent_name, status.status, status.phase, status.detail, logs
    );
    for c in &status.components {
        if matches!(
            c.status,
            WorkflowStatus::Failure | WorkflowStatus::Timeout | WorkflowStatus::Aborted
        ) {
            let logs = if c.url.is_empty() { "n/a" } else { c.url.as_str() };
            error!("  component {}: {} — logs: {}", c.name, c.status, logs);
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
        if let Some(ref status) = iso_agent_status {
            log_agent_failure("iso-agent", status);
        }
        if let Some(ref status) = xoa_vm_agent_status {
            log_agent_failure("xoa-vm-agent", status);
        }
        llm_hint = Some("Verify network interfaces and upstream tag validity fields.".to_string());
    }

    // 3. Formulate structural history tracking context
    let mut history_item = RunHistoryItem::default();
    history_item.timestamp = Utc::now();
    history_item.llm_hint = llm_hint;

    if let Some(ref status) = iso_agent_status {
        (history_item.xolite_status, history_item.xolite_url) =
            component_or_agent(status, "xolite-ce");
        (history_item.xoa_proxy_status, history_item.xoa_proxy_url) =
            component_or_agent(status, "xoa-proxy");
        (history_item.iso_status, history_item.iso_url) = component_or_agent(status, "iso");
    }

    if let Some(ref status) = xoa_vm_agent_status {
        (history_item.xoa_hl_status, history_item.xoa_hl_url) =
            component_or_agent(status, "xoa-hl");
        (history_item.xoa_image_status, history_item.xoa_image_url) =
            component_or_agent(status, "xoa-image");
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

/// Badge class for the String statuses stored in history (WorkflowStatus Display values).
fn badge_class(status: &str) -> &'static str {
    match status {
        "Success" => "success",
        "Failure" | "Timeout" | "Aborted" => "failure",
        _ => "progress",
    }
}

fn render_dashboard_html(history: &[RunHistoryItem]) -> String {
    let mut html = String::from(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>XCP-ng Engine Pipeline</title>
    <style>body{font-family:sans-serif;background:#121214;color:#e1e1e6;margin:40px;} h1{color:#4f46e5;}
    .card{background:#1c1c1f;padding:20px;border-radius:8px;margin-bottom:20px;border:1px solid #2d2d34;}
    .badge{padding:4px 8px;border-radius:4px;font-size:12px;font-weight:bold;}
    .success{background:#166534;color:#bbf7d0;} .failure{background:#991b1b;color:#fca5a5;} .progress{background:#854d0e;color:#fef08a;}
    a{color:#6366f1;text-decoration:none;} a:hover{text-decoration:underline;} pre{background:#09090b;padding:15px;border-radius:6px;overflow-x:auto;color:#fda4af;border-left:4px solid #f43f5e;}</style></head><body>
    <h1>XCP-ng Agentic Build Dashboard</h1>"#,
    );

    for (idx, item) in history.iter().enumerate() {
        let title_prefix = if idx == 0 { "Latest Execution Run" } else { "Historical Archive" };
        html.push_str(&format!(
            r#"<div class="card"><h3>{} ({})</h3>"#,
            title_prefix,
            item.timestamp
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
        ));

        let rows: [(&str, &str, &str); 5] = [
            ("Xolite-ce", &item.xolite_status, &item.xolite_url),
            ("XOA-Proxy", &item.xoa_proxy_status, &item.xoa_proxy_url),
            ("ISO Matrix", &item.iso_status, &item.iso_url),
            ("XOA-HL", &item.xoa_hl_status, &item.xoa_hl_url),
            ("XOA Image", &item.xoa_image_status, &item.xoa_image_url),
        ];
        for (label, status, url) in rows {
            html.push_str(&format!(
                r#"<p><b>{}:</b> <span class="badge {}">{}</span> | <a href="{}" target="_blank">Logs</a></p>"#,
                label,
                badge_class(status),
                status,
                url
            ));
        }

        if let Some(ref hint) = item.llm_hint {
            html.push_str(&format!(
                r#"<h4>🤖 Qwen-Coder Diagnostic Remediation Analysis:</h4><pre>{}</pre>"#,
                hint
            ));
        }
        html.push_str("</div>");
    }

    html.push_str("</body></html>");
    html
}

async fn render_dashboard_async() -> Result<(), OrchestratorError> {
    fs::create_dir_all(TARGET_REPORT_DIR).await?;
    let history: Vec<RunHistoryItem> = load_json_with_default(HISTORY_FILE).await.unwrap_or_default();

    let html = render_dashboard_html(&history);

    let output_path = format!("{}/build_report.html", TARGET_REPORT_DIR);
    fs::write(&output_path, html).await?;
    info!("Dashboard rendered to {}", output_path);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item() -> RunHistoryItem {
        RunHistoryItem {
            xolite_status: "Success".to_string(),
            xolite_url: "https://github.com/Vagrantin/xolite-ce/actions/runs/1".to_string(),
            xoa_proxy_status: "Failure".to_string(),
            xoa_proxy_url: "https://github.com/Vagrantin/xoa-proxy/actions/runs/2".to_string(),
            iso_status: "In Progress".to_string(),
            xoa_hl_status: "Success".to_string(),
            xoa_hl_url: "https://github.com/Vagrantin/xoa-hl/actions/runs/3".to_string(),
            xoa_image_status: "Success".to_string(),
            xoa_image_url: "https://github.com/Vagrantin/xoa-hl/releases/tag/v1".to_string(),
            llm_hint: Some("check the spec file".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn dashboard_renders_all_component_rows_with_links() {
        let html = render_dashboard_html(&[sample_item()]);
        for label in ["Xolite-ce", "XOA-Proxy", "ISO Matrix", "XOA-HL", "XOA Image"] {
            assert!(html.contains(label), "missing row {}", label);
        }
        assert!(html.contains(r#"href="https://github.com/Vagrantin/xolite-ce/actions/runs/1""#));
        assert!(html.contains(r#"href="https://github.com/Vagrantin/xoa-hl/releases/tag/v1""#));
        assert!(html.contains(r#"class="badge success">Success"#));
        assert!(html.contains(r#"class="badge failure">Failure"#));
        assert!(html.contains(r#"class="badge progress">In Progress"#));
        assert!(html.contains("check the spec file"));
    }

    #[test]
    fn badge_classes_map_display_strings() {
        assert_eq!(badge_class("Success"), "success");
        assert_eq!(badge_class("Failure"), "failure");
        assert_eq!(badge_class("Timeout"), "failure");
        assert_eq!(badge_class("Aborted"), "failure");
        assert_eq!(badge_class("Skipped"), "progress");
        assert_eq!(badge_class("In Progress"), "progress");
    }
}
