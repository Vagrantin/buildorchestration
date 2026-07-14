//! GitHub API helpers and client functions.

use super::*;
use reqwest::Client;
//use std::str::FromUtf8Error;
use std::time::Instant;

/// Create a GitHub client with authentication token
pub fn create_github_client(token: &str) -> Result<Client, OrchestratorError> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Authorization",
        format!("Bearer {}", token)
            .parse()
            .map_err(|e: reqwest::header::InvalidHeaderValue| {
                OrchestratorError::GitHubApi("header creation".to_string(), e.to_string())
            })?,
    );
    headers.insert(
        "User-Agent",
        "XCP-Orchestrator-Rust-Agent"
            .parse()
            .map_err(|e: reqwest::header::InvalidHeaderValue| {
                OrchestratorError::GitHubApi("header creation".to_string(), e.to_string())
            })?,
    );
    headers.insert(
        "Accept",
        "application/vnd.github+json"
            .parse()
            .map_err(|e: reqwest::header::InvalidHeaderValue| {
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

   serde_json::from_str(&body)
        .map_err(|e| OrchestratorError::Json(e))


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

            match next_tag_candidate(&tag) {
                Some(next) => {
                    tracing::warn!("Tag {} already exists, retrying with {}", tag, next);
                    tag = next;
                    continue;
                }
                None => {
                    return Err(OrchestratorError::TagCreation(format!(
                        "Tag {} already exists and no increment strategy applies to it",
                        tag
                    )));
                }
            }
        }

        return Err(OrchestratorError::GitHubApi(
            format!("create_and_push_tag for {}", repo),
            format!("Failed to create tag {}: {}", tag, body),
        ));
    }
}

/// Fetch the latest published release of a repo and resolve its tag to a
/// commit SHA. Returns `Ok(None)` when the repo has no releases yet.
///
/// This is the ground truth for "what did we last build successfully" — unlike
/// the local version-state files it survives state loss and failed runs.
pub async fn fetch_latest_release_ref(
    client: &Client,
    repo: &str,
) -> Result<Option<(String, String)>, OrchestratorError> {
    let url = format!("https://api.github.com/repos/{}/{}/releases/latest", OWNER, repo);
    let res = client
        .get(&url)
        .send()
        .await
        .map_err(|e| OrchestratorError::GitHubApi(format!("latest release for {}", repo), e.to_string()))?;

    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let release: GHRelease =
        parse_github_response(res, &format!("fetch_latest_release_ref for {}", repo)).await?;
    let sha = fetch_tag_commit_sha(client, repo, &release.tag_name).await?;
    Ok(Some((release.tag_name, sha)))
}

/// Resolve a tag name to the commit SHA it points at, dereferencing annotated
/// tags (the agents push lightweight tags, so the extra hop is a fallback).
pub async fn fetch_tag_commit_sha(
    client: &Client,
    repo: &str,
    tag: &str,
) -> Result<String, OrchestratorError> {
    #[derive(Deserialize)]
    struct RefObject {
        sha: String,
        #[serde(rename = "type")]
        object_type: String,
    }
    #[derive(Deserialize)]
    struct RefResp {
        object: RefObject,
    }

    let url = format!("https://api.github.com/repos/{}/{}/git/ref/tags/{}", OWNER, repo, tag);
    let resp: RefResp = parse_github_response(
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(tag.to_string(), e.to_string()))?,
        &format!("fetch_tag_commit_sha for {} on {}", tag, repo),
    )
    .await?;

    if resp.object.object_type != "tag" {
        return Ok(resp.object.sha);
    }

    let deref_url = format!(
        "https://api.github.com/repos/{}/{}/git/tags/{}",
        OWNER, repo, resp.object.sha
    );
    let deref: RefResp = parse_github_response(
        client
            .get(&deref_url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(tag.to_string(), e.to_string()))?,
        &format!("dereference annotated tag {} on {}", tag, repo),
    )
    .await?;
    Ok(deref.object.sha)
}

/// An uploaded release asset.
#[derive(Deserialize, Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// A published release, trimmed to what the agents need to decide whether an
/// artefact already exists.
#[derive(Deserialize, Debug, Clone)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// List a repo's releases, newest first.
pub async fn fetch_releases(
    client: &Client,
    repo: &str,
    per_page: u8,
) -> Result<Vec<ReleaseInfo>, OrchestratorError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases?per_page={}",
        OWNER, repo, per_page
    );
    parse_github_response(
        client
            .get(&url)
            .send()
            .await
            .map_err(|e| OrchestratorError::GitHubApi(format!("releases for {}", repo), e.to_string()))?,
        &format!("fetch_releases for {}", repo),
    )
    .await
}

/// Parse a `v{version}-ce{N}` tag (xolite-ce, xcp-ng-ce-iso) into
/// `(version, ce_counter)`, e.g. `v0.21.0-ce6` → `("0.21.0", 6)`.
pub fn parse_ce_tag(tag: &str) -> Option<(String, u32)> {
    let body = tag.strip_prefix('v')?;
    let ce_pos = body.rfind("-ce")?;
    let counter: u32 = body[ce_pos + 3..].parse().ok()?;
    let version = &body[..ce_pos];
    if version.is_empty() {
        return None;
    }
    Some((version.to_string(), counter))
}

/// Parse a plain xoa-proxy tag `v{X.Y.Z}` or `v{X.Y.Z.N}` into
/// `(version, counter)`, e.g. `v0.1.1` → `("0.1.1", 0)`, `v0.1.1.3` → `("0.1.1", 3)`.
pub fn parse_plain_version_tag(tag: &str) -> Option<(String, u32)> {
    let segments: Vec<&str> = tag.strip_prefix('v')?.split('.').collect();
    if !segments
        .iter()
        .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    match segments.len() {
        3 => Some((segments.join("."), 0)),
        4 => Some((segments[..3].join("."), segments[3].parse().ok()?)),
        _ => None,
    }
}

/// Compute the next tag to try when `tag` already exists on the remote.
///
/// Two tag schemes are in use:
/// - `-ceN` suffix (xolite-ce, xcp-ng-ce-iso): `v0.23.0-ce1` → `v0.23.0-ce2`
/// - plain version tags (xoa-proxy, which carries no `-ce`): a fourth numeric
///   segment acts as the collision counter, matching the PatchBump format
///   `v{version}.{counter}`: `v0.1.1` → `v0.1.1.1`, `v0.1.1.2` → `v0.1.1.3`
///
/// Returns `None` when the tag matches neither scheme.
fn next_tag_candidate(tag: &str) -> Option<String> {
    if let Some(ce_pos) = tag.rfind("-ce") {
        let (prefix, counter_str) = tag.split_at(ce_pos + 3);
        return counter_str
            .parse::<u32>()
            .ok()
            .map(|counter| format!("{}{}", prefix, counter + 1));
    }

    let segments: Vec<&str> = tag.strip_prefix('v')?.split('.').collect();
    if !segments
        .iter()
        .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    match segments.len() {
        3 => Some(format!("{}.1", tag)),
        4 => {
            let counter: u32 = segments[3].parse().ok()?;
            Some(format!(
                "v{}.{}.{}.{}",
                segments[0],
                segments[1],
                segments[2],
                counter + 1
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{next_tag_candidate, parse_ce_tag, parse_pinned_xolite_tag, parse_plain_version_tag};

    #[test]
    fn ce_tags_parse() {
        assert_eq!(parse_ce_tag("v0.21.0-ce6"), Some(("0.21.0".to_string(), 6)));
        assert_eq!(parse_ce_tag("v8.3-ce9"), Some(("8.3".to_string(), 9)));
        assert_eq!(parse_ce_tag("v0.1.1"), None);
        assert_eq!(parse_ce_tag("v-ce3"), None);
        assert_eq!(parse_ce_tag("v1-cebad"), None);
    }

    #[test]
    fn plain_version_tags_parse() {
        assert_eq!(parse_plain_version_tag("v0.1.1"), Some(("0.1.1".to_string(), 0)));
        assert_eq!(parse_plain_version_tag("v0.1.1.3"), Some(("0.1.1".to_string(), 3)));
        assert_eq!(parse_plain_version_tag("v0.21.0-ce6"), None);
        assert_eq!(parse_plain_version_tag("v5.113.2_e281c536"), None);
        assert_eq!(parse_plain_version_tag("v0.1"), None);
    }

    #[test]
    fn ce_suffix_increments() {
        assert_eq!(next_tag_candidate("v0.23.0-ce1").as_deref(), Some("v0.23.0-ce2"));
        assert_eq!(next_tag_candidate("v8.3-ce12").as_deref(), Some("v8.3-ce13"));
    }

    #[test]
    fn plain_version_gains_counter_segment() {
        assert_eq!(next_tag_candidate("v0.1.1").as_deref(), Some("v0.1.1.1"));
    }

    #[test]
    fn counter_segment_increments() {
        assert_eq!(next_tag_candidate("v0.1.1.2").as_deref(), Some("v0.1.1.3"));
    }

    #[test]
    fn unrecognized_schemes_return_none() {
        assert_eq!(next_tag_candidate("release-foo"), None);
        assert_eq!(next_tag_candidate("v1-cebad"), None);
        assert_eq!(next_tag_candidate("v8.3-ce202605.5"), None);
        assert_eq!(next_tag_candidate("v0.1"), None);
    }

    #[test]
    fn pinned_xolite_tags_parse() {
        assert_eq!(parse_pinned_xolite_tag("xo-lite-v0.21.0\n"), Some("0.21.0".to_string()));
        assert_eq!(parse_pinned_xolite_tag("  xo-lite-v0.23.0  "), Some("0.23.0".to_string()));
        assert_eq!(parse_pinned_xolite_tag(""), None);
        assert_eq!(parse_pinned_xolite_tag("xo-lite-v"), None);
        assert_eq!(parse_pinned_xolite_tag("v0.21.0"), None);
        assert_eq!(parse_pinned_xolite_tag("xo-server-v5.113.2"), None);
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

/// Locate a workflow run triggered by a `workflow_dispatch` API call.
///
/// Unlike `locate_tag_triggered_run` (which matches on `head_branch` for
/// tag-push events), `workflow_dispatch` runs are filed under
/// `event=workflow_dispatch` and carry no tag to match against — so the only
/// reliable signal is `created_at` relative to the moment we dispatched.
pub async fn locate_dispatch_triggered_run(
    client: &Client,
    repo: &str,
    trigger_marker: DateTime<Utc>,
) -> Result<(u64, String), OrchestratorError> {
    let check_url = format!(
        "https://api.github.com/repos/{}/{}/actions/runs?event=workflow_dispatch&per_page=10",
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
                .map_err(|e| OrchestratorError::GitHubApi(repo.to_string(), e.to_string()))?,
            &format!("locate_dispatch_triggered_run for {}", repo),
        )
        .await?;

        for run in response.workflow_runs {
            if run.created_at >= (trigger_marker - Duration::from_secs(5)) {
                return Ok((run.id, run.html_url));
            }
        }
    }

    Err(OrchestratorError::WorkflowRunNotFound(format!(
        "Timeout waiting for workflow_dispatch-triggered run in repo {}",
        repo
    )))
}

/// Dispatch a workflow via the `workflow_dispatch` API.
///
/// `git_ref` may be a branch or a tag — dispatching on a tag makes
/// `github.ref_name` inside the run resolve to that tag, which the ISO
/// workflow relies on to derive its release version.
pub async fn dispatch_workflow(
    client: &Client,
    repo: &str,
    workflow_file: &str,
    git_ref: &str,
    inputs: serde_json::Value,
) -> Result<(), OrchestratorError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/actions/workflows/{}/dispatches",
        OWNER, repo, workflow_file
    );
    let payload = serde_json::json!({ "ref": git_ref, "inputs": inputs });

    let res = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| OrchestratorError::GitHubApi(format!("dispatch_workflow for {}", repo), e.to_string()))?;

    // workflow_dispatch returns 204 No Content on success
    if res.status() != reqwest::StatusCode::NO_CONTENT {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        return Err(OrchestratorError::GitHubApi(
            format!("dispatch_workflow for {}", repo),
            format!("workflow_dispatch returned {} — {}", code, body),
        ));
    }

    tracing::info!("Dispatched {} on {} (ref {})", workflow_file, repo, git_ref);
    Ok(())
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
        .map_err(|e| OrchestratorError::Json(e))?;

    Ok(pkg["version"]
        .as_str()
        .ok_or_else(|| OrchestratorError::VersionFormat("package.json missing version field".into()))?
        .to_string())
}

/// Parse the contents of xolite-ce's UPSTREAM_TAG pin file into the bare
/// version part of the tag ("xo-lite-v0.21.0" -> "0.21.0").
pub fn parse_pinned_xolite_tag(content: &str) -> Option<String> {
    let version = content.trim().strip_prefix("xo-lite-v")?;
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// Fetch the pinned upstream XO Lite tag from the UPSTREAM_TAG file at the root
/// of the xolite-ce repo. Returns Ok(None) when the pin file does not exist so
/// callers can fall back to the latest upstream release.
pub async fn fetch_pinned_xolite_tag(client: &Client) -> Result<Option<String>, OrchestratorError> {
    let url = format!(
        "https://api.github.com/repos/{}/xolite-ce/contents/UPSTREAM_TAG?ref={}",
        OWNER, DEFAULT_BRANCH
    );
    let res = client
        .get(&url)
        .send()
        .await
        .map_err(|e| OrchestratorError::GitHubApi("fetch UPSTREAM_TAG".to_string(), e.to_string()))?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let file: GHFileContent = parse_github_response(res, "fetch_pinned_xolite_tag").await?;

    use base64::{engine::general_purpose, Engine as _};
    let cleaned: String = file.content.chars().filter(|c| !c.is_whitespace()).collect();
    let decoded = general_purpose::STANDARD
        .decode(cleaned)
        .map_err(|e| OrchestratorError::Base64Decode(e.to_string()))?;
    let content = String::from_utf8(decoded)
        .map_err(|e| OrchestratorError::VersionFormat(format!("UPSTREAM_TAG is not UTF-8: {}", e)))?;

    parse_pinned_xolite_tag(&content).map(Some).ok_or_else(|| {
        OrchestratorError::VersionFormat(format!(
            "UPSTREAM_TAG does not contain an xo-lite-v* tag: {:?}",
            content.trim()
        ))
    })
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
        .map_err(|e| OrchestratorError::FromUtf8(e))?;

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
        .map_err(|e| OrchestratorError::FromUtf8(e))?;

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
