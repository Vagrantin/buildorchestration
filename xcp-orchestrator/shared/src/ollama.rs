//! Ollama API integration for LLM diagnostics.

use super::*;
use reqwest::Client;

/// Request payload for Ollama API
#[derive(Serialize)]
pub struct OllamaRequest {
    pub model: String,
    pub prompt: String,
    pub stream: bool,
}

/// Response from Ollama API
#[derive(Deserialize)]
pub struct OllamaResponse {
    pub response: String,
}

/// Evaluate build logs using Ollama LLM
pub async fn evaluate_log_via_ollama(client: &Client, raw_logs: &str) -> Result<String, OrchestratorError> {
    let prompt = format!(
        "You are an expert XCP-ng software integration engineer. Analyze the following failed build logs.\n\
        Pinpoint the exact reason for failure (e.g., missing dependencies, network drops, compilation error).\n\
        Provide highly concise, actionable hints to remediate the setup.\n\n\
        ### BUILD LOG EXCERPT:\n{}\n\n### REMEDIATION HINT:",
        raw_logs
    );

    let payload = OllamaRequest {
        model: MODEL_NAME.to_string(),
        prompt,
        stream: false,
    };

    let res = client
        .post(OLLAMA_URL)
        .json(&payload)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| OrchestratorError::OllamaError(e.to_string()))?;

    let json_res = res
        .json::<OllamaResponse>()
        .await
        .map_err(|e| OrchestratorError::OllamaError(e.to_string()))?;

    Ok(json_res.response)
}
