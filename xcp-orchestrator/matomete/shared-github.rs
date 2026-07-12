//! GitHub API helpers and client functions.

use super::*;
use reqwest::Client;
use std::str::FromUtf8Error;
use std::time::Instant;

/// Create a GitHub client with authentication token
pub fn create_github_client(token: &str) -> Result<Client, OrchestratorError> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Authorization",
        format!("Bearer {}", token)
            .parse()
            .map_err(|e: std::str::Utf8Error| {
                OrchestratorError::GitHubApi("header creation".to_string(), e.to_string())
            })?,
    );
    headers.insert(
        "User-Agent",
        "XCP-Orchestrator-Rust-Agent"
            .parse()
            .map_err(|e: std::str::Utf8Error| {
                OrchestratorError::GitHubApi("header creation".to_string(), e.to_string())
            })?,
    );
    headers.insert(
        "Accept",
        "application/vnd.github+json"
            .parse()
            .map_err(|e: std::str::Utf8Error| {
                OrchestratorError::GitHubApi("header creation".to_string(), e.to_string())
            })?,
    );

    let client = Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| OrchestratorError::GitHubApi("client build".to_string(), e.to_string()))?;

    Ok(client)
}

/// Parse a GitHub API response, handling errors appropriately
pub async fn parse_github_response<T: serde::de::DeserializeOwned>(
    res: reqwest::Response,
    context: &str,
) -> Result<T, OrchestratorError> {
    let status = res.status();
    let body = res
        .text()
        .await
        .map_err(|e| OrchestratorError::GitHubApi(context.to_string(), e.to_string()))?;

    if !status.is_success() {
        return Err(OrchestratorError::GitHubApi(
            format!("{} (status {})", context, status),
            body,
        ));
    }

    serde_json::from_str(&body).map_err(|e| {
        OrchestratorError::JsonSerialization(format!(
            "Failed to parse JSON during {} (status {}): {}\nBody (first 500 chars): {}",
            context,
            status,
            e,
            &body[..body.len().min(500)]
        ))
    })
}

/// Fetch the HEAD SHA of a repository
pub async fn fetch_repo_head_sha(client: &Client, repo: &str) -> Result<String, OrchestratorError> {
    let url = format!("https://api.github.com/repos/{}/{}/commits/{}", OWNER, repo, DEFAULT_BRANCH);

    #[derive(Deserialize)]
    struct CommitResp {
        sha: String,
    }

    let resp: CommitResp = parse_github_response(
        client.get(&url).send().await.map_err(|e| {
            OrchestratorError::GitHubApi(
                format!("fetch_repo_head_sha for {}", repo),
                e.to_string(),
            )
        })?,
        &format!("fetch_repo_head_sha for {}", repo),
    )
    .await?;

    Ok(resp.sha)
}

/// Create and push a tag to a repository
pub async fn create_and_push_tag(
    client: &Client,
    repo: &str,
    base_tag: &str,
    sha: &str,
) -> Result<String, OrchestratorError> {
    let mut tag = base_tag.to_string();
    let mut attempt = 0;
    const MAX_ATTEMPTS: u32 = 99;

    loop {
        let url = format!("https://api.github.com/repos/{}/{}/git/refs", OWNER, repo);
        let payload = serde_json::json!({
            "ref": format!("refs/tags/{}", tag),
            "sha": sha,
        });

        let res = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(tag.clone(), e.to_string()))?;

        if res.status().is_success() {
            tracing::info!("Pushed tag {} on {} (sha {})", tag, repo, &sha[..7.min(sha.len())]);
            return Ok(tag);
        }

        let status = res.status();
        let body = res
            .text()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(tag.clone(), e.to_string()))?;

        if status == 422 && body.contains("Reference already exists") {
            attempt += 1;
            if attempt >= MAX_ATTEMPTS {
                return Err(OrchestratorError::TagCreation(format!(
                    "Failed to create tag on {}: tried {} through {} (all exist), giving up",
                    repo, base_tag, tag
                )));
            }

            if let Some(ce_pos) = tag.rfind("-ce") {
                let prefix = &tag[..ce_pos + 3];
                let counter_str = &tag[ce_pos + 3..];
                if let Ok(counter) = counter_str.parse::<u32>() {
                    tag = format!("{}{}", prefix, counter + 1);
                    tracing::warn!(
                        "Tag {} already exists, retrying with {}",
                        &tag[..tag.len() - 1],
                        tag
                    );
                    continue;
                }
            }
            return Err(OrchestratorError::TagCreation(format!(
                "Tag {} already exists and could not parse counter suffix to increment",
                tag
            )));
        }

        return Err(OrchestratorError::GitHubApi(
            format!("create_and_push_tag for {}", repo),
            format!("Failed to create tag {}: {}", tag, body),
        ));
    }
}

/// Locate a workflow run triggered by a tag push
pub async fn locate_tag_triggered_run(
    client: &Client,
    repo: &str,
    tag: &str,
    trigger_marker: DateTime<Utc>,
) -> Result<(u64, String), OrchestratorError> {
    let check_url = format!(
        "https://api.github.com/repos/{}/{}/actions/runs?event=push&per_page=5",
        OWNER, repo
    );
    let timeout_limit = Instant::now() + Duration::from_secs(180);

    while Instant::now() < timeout_limit {
        tokio::time::sleep(Duration::from_secs(6)).await;
        let response: GHRunsResponse = parse_github_response(
            client
                .get(&check_url)
                .send()
                .await
                .map_err(|e| OrchestratorError::GitHubApi(tag.to_string(), e.to_string()))?,
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

    Err(OrchestratorError::WorkflowRunNotFound(format!(
        "Timeout waiting for tag-push run matching {} in repo {}",
        tag, repo
    )))
}

/// Query the conclusion of a workflow run
pub async fn query_run_conclusion(
    client: &Client,
    repo: &str,
    run_id: u64,
) -> Result<String, OrchestratorError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/actions/runs/{}",
        OWNER, repo, run_id
    );
    let run: GHRun = parse_github_response(
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(run_id.to_string(), e.to_string()))?,
        "query_run_conclusion",
    )
    .await?;

    if run.status == "completed" {
        Ok(run.conclusion.unwrap_or_else(|| "unknown".to_string()))
    } else {
        Ok("In Progress".to_string())
    }
}

/// Extract log context from a failed job
pub async fn extract_failed_log_context(
    client: &Client,
    repo: &str,
    run_id: u64,
) -> Result<String, OrchestratorError> {
    let jobs_url = format!(
        "https://api.github.com/repos/{}/{}/actions/runs/{}/jobs",
        OWNER, repo, run_id
    );
    let res: GHJobsResponse = parse_github_response(
        client
            .get(&jobs_url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(run_id.to_string(), e.to_string()))?,
        "extract_failed_log_context jobs list",
    )
    .await?;

    if let Some(failed_job) = res.jobs.iter().find(|j| j.conclusion.as_deref() == Some("failure")) {
        let log_url = format!(
            "https://api.github.com/repos/{}/{}/actions/jobs/{}/logs",
            OWNER, repo, failed_job.id
        );
        let log_text = client
            .get(&log_url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(run_id.to_string(), e.to_string()))?
            .text()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(run_id.to_string(), e.to_string()))?;

        let lines: Vec<&str> = log_text.lines().collect();
        let tail_count = lines.len().min(250);
        let truncated = lines[lines.len() - tail_count..].join("\n");
        return Ok(truncated);
    }

    Ok("Could not resolve failed job log metrics.".to_string())
}

/// Fetch the latest upstream XO Lite tag
pub async fn fetch_latest_upstream_xolite_tag(client: &Client) -> Result<String, OrchestratorError> {
    let url = "https://api.github.com/repos/vatesfr/xen-orchestra/releases?per_page=10";
    let releases: Vec<GHRelease> = parse_github_response(
        client
            .get(url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi("fetch releases".to_string(), e.to_string()))?,
        "fetch_latest_upstream_xolite_tag",
    )
    .await?;

    releases
        .into_iter()
        .filter(|r| r.tag_name.starts_with("xo-lite-v") && !r.assets.is_empty())
        .find_map(|r| r.tag_name.strip_prefix("xo-lite-v").map(String::from))
        .ok_or_else(|| {
            OrchestratorError::VersionFormat(
                "No released xo-lite-v* tag with assets found upstream".to_string(),
            )
        })
}

/// Fetch the upstream XO Lite version from package.json
pub async fn fetch_upstream_xolite_version(
    client: &Client,
    upstream_tag: &str,
) -> Result<String, OrchestratorError> {
    let url = format!(
        "https://api.github.com/repos/vatesfr/xen-orchestra/contents/@xen-orchestra/lite/package.json?ref=xo-lite-v{}",
        upstream_tag
    );

    #[derive(Deserialize)]
    struct ContentResp {
        content: String,
    }

    let resp: ContentResp = parse_github_response(
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(upstream_tag.to_string(), e.to_string()))?,
        &format!("fetch_upstream_xolite_version for tag xo-lite-v{}", upstream_tag),
    )
    .await?;

    use base64::{engine::general_purpose, Engine as _};
    let cleaned: String = resp.content.chars().filter(|c| !c.is_whitespace()).collect();
    let decoded = general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|e| OrchestratorError::Base64Decode(e.to_string()))?;
    let pkg: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|e| OrchestratorError::JsonSerialization(e.to_string()))?;

    Ok(pkg["version"]
        .as_str()
        .ok_or_else(|| OrchestratorError::VersionFormat("package.json missing version field".into()))?
        .to_string())
}

/// Fetch xoa-proxy version from Cargo.toml
pub async fn fetch_xoa_proxy_version(client: &Client) -> Result<String, OrchestratorError> {
    let url = format!(
        "https://api.github.com/repos/{}/xoa-proxy/contents/Cargo.toml?ref={}",
        OWNER, DEFAULT_BRANCH
    );
    let cargo_content: GHFileContent = parse_github_response(
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi("fetch Cargo.toml".to_string(), e.to_string()))?,
        "decide_xoa_proxy_bump Cargo.toml",
    )
    .await?;

    use base64::{engine::general_purpose, Engine as _};
    let cleaned: String = cargo_content.content.chars().filter(|c| !c.is_whitespace()).collect();
    let cargo_toml_bytes = general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|e| OrchestratorError::Base64Decode(e.to_string()))?;
    let cargo_toml = String::from_utf8(cargo_toml_bytes)
        .map_err(|e| OrchestratorError::FromUtf8(e.to_string()))?;

    cargo_toml
        .lines()
        .find(|line| line.starts_with("version"))
        .and_then(|line| {
            line.split('"')
                .nth(1)
                .map(|v| v.to_string())
        })
        .ok_or_else(|| OrchestratorError::VersionFormat("Could not parse version from Cargo.toml".into()))
}

/// Append an entry to the release matrix
pub async fn append_release_matrix_entry(
    client: &Client,
    iso_tag: &str,
    xolite_version: &str,
    xolite_upstream: &str,
    xoa_proxy_version: &str,
) -> Result<(), OrchestratorError> {
    use base64::{engine::general_purpose, Engine as _};

    let url = format!(
        "https://api.github.com/repos/{}/{}/contents/{}",
        OWNER, DOCS_REPO, RELEASES_DATA_PATH
    );
    let existing: GHFileContent = parse_github_response(
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi("append_release_matrix_entry GET".to_string(), e.to_string()))?,
        "append_release_matrix_entry GET",
    )
    .await?;

    let cleaned: String = existing.content.chars().filter(|c| !c.is_whitespace()).collect();
    let current_yaml_bytes = general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|e| OrchestratorError::Base64Decode(e.to_string()))?;
    let current_yaml = String::from_utf8(current_yaml_bytes)
        .map_err(|e| OrchestratorError::FromUtf8(e.to_string()))?;

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

    let res = client
        .put(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| OrchestratorError::GitHubApi(iso_tag.to_string(), e.to_string()))?;

    if !res.status().is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(OrchestratorError::GitHubApi(
            iso_tag.to_string(),
            format!("Failed to update release matrix: {}", body),
        ));
    }

    tracing::info!("Release matrix updated with {} (push will trigger pages.yml)", iso_tag);
    Ok(())
}
