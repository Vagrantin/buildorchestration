//! orchestrator-api - small local HTTP endpoint that lets the dashboard's
//! "Run now" buttons start an agent's systemd unit on demand.
//!
//! Binds to 127.0.0.1 only; a reverse proxy (e.g. nginx, alongside the static
//! dashboard in TARGET_REPORT_DIR) is expected to expose /api/ externally.
//! Every request must carry `Authorization: Bearer <token>` matching the
//! TRIGGER_TOKEN credential, since a match starts a root systemd unit.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::Serialize;
use shared::load_credential;
use std::sync::Arc;
use tokio::process::Command;
use tracing::{info, warn};

const BIND_ADDR: &str = "127.0.0.1:8787";

/// Agent name (as used by the dashboard buttons) to its systemd unit.
/// Kept as an explicit table rather than interpolating the path segment
/// directly into a systemctl call. "orchestrator" installs as
/// xcp-orchestrator.service (see systemd/xcp-orchestrator.service).
const ALLOWED_AGENTS: &[(&str, &str)] = &[
    ("orchestrator", "xcp-orchestrator.service"),
    ("iso-agent", "iso-agent.service"),
    ("xoa-vm-agent", "xoa-vm-agent.service"),
];

fn unit_for_agent(agent: &str) -> Option<&'static str> {
    ALLOWED_AGENTS
        .iter()
        .find(|(name, _)| *name == agent)
        .map(|(_, unit)| *unit)
}

struct AppState {
    token: String,
}

#[derive(Serialize)]
struct TriggerResponse {
    agent: String,
    started: bool,
    detail: String,
}

fn is_authorized(headers: &HeaderMap, expected_token: &str) -> bool {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value
        .strip_prefix("Bearer ")
        .map(|t| t == expected_token)
        .unwrap_or(false)
}

async fn trigger_agent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(agent): Path<String>,
) -> (StatusCode, Json<TriggerResponse>) {
    if !is_authorized(&headers, &state.token) {
        warn!("Rejected unauthorized trigger request for '{}'", agent);
        return (
            StatusCode::UNAUTHORIZED,
            Json(TriggerResponse {
                agent,
                started: false,
                detail: "missing or invalid bearer token".to_string(),
            }),
        );
    }

    let Some(unit) = unit_for_agent(&agent) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(TriggerResponse {
                agent: agent.clone(),
                started: false,
                detail: format!("unknown agent '{}'", agent),
            }),
        );
    };
    info!("Manual trigger: starting {}", unit);

    let result = Command::new("systemctl")
        .args(["start", unit])
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => (
            StatusCode::OK,
            Json(TriggerResponse {
                agent,
                started: true,
                detail: format!("{} started", unit),
            }),
        ),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            warn!("systemctl start {} failed: {}", unit, stderr);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(TriggerResponse {
                    agent,
                    started: false,
                    detail: stderr,
                }),
            )
        }
        Err(e) => {
            warn!("Failed to spawn systemctl for {}: {}", unit, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(TriggerResponse {
                    agent,
                    started: false,
                    detail: e.to_string(),
                }),
            )
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("orchestrator_api=info")
        .init();

    let token = load_credential("TRIGGER_TOKEN")
        .expect("TRIGGER_TOKEN credential required (LoadCredential=TRIGGER_TOKEN=...)");
    let state = Arc::new(AppState { token });

    let app = Router::new()
        .route("/api/trigger/:agent", post(trigger_agent))
        .with_state(state);

    info!("orchestrator-api listening on {}", BIND_ADDR);
    let listener = tokio::net::TcpListener::bind(BIND_ADDR)
        .await
        .expect("failed to bind orchestrator-api listener");
    axum::serve(listener, app)
        .await
        .expect("orchestrator-api server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn authorized_only_with_matching_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret123"),
        );
        assert!(is_authorized(&headers, "secret123"));
        assert!(!is_authorized(&headers, "other"));
    }

    #[test]
    fn missing_header_is_unauthorized() {
        assert!(!is_authorized(&HeaderMap::new(), "secret123"));
    }

    #[test]
    fn allowed_agents_map_to_their_actual_systemd_units() {
        assert_eq!(unit_for_agent("orchestrator"), Some("xcp-orchestrator.service"));
        assert_eq!(unit_for_agent("iso-agent"), Some("iso-agent.service"));
        assert_eq!(unit_for_agent("xoa-vm-agent"), Some("xoa-vm-agent.service"));
        assert_eq!(unit_for_agent("something-else"), None);
    }
}
