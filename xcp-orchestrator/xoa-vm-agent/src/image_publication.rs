//! Image build identity is separate from either source repository's history.
use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

pub fn image_tag_at(source: &str, at: DateTime<Utc>) -> String {
    let short = &source[..7.min(source.len())];
    format!("xoa-image-{}-{}", at.format("%Y%m%d-%H%M%S%9f"), short)
}

pub fn manifest(tag: &str, source: &str, builder: &str, rpm: &str, rpm_asset: &str) -> Value {
    json!({
        "schema": 1, "image_tag": tag,
        "source_commits": { "xoa-hl": source, "build-xoa-hl": builder },
        "rpm_tag": rpm, "rpm_asset": rpm_asset
    })
}

fn sha(value: &Value) -> Result<String> {
    let sha = value["sha"]
        .as_str()
        .context("GitHub object response lacks sha")?;
    if sha.len() != 40 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("Invalid GitHub object sha");
    }
    Ok(sha.to_owned())
}

/// No branch or existing tag is changed. The new commit is reachable from its release tag.
pub async fn create_provenance_commit(
    client: &reqwest::Client,
    repo_api: &str,
    source_sha: &str,
    build: &Value,
) -> Result<String> {
    let base: Value = client
        .get(format!("{repo_api}/git/commits/{source_sha}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let base_tree = sha(&base["tree"])?;
    let tree: Value = client
        .post(format!("{repo_api}/git/trees"))
        .json(&json!({"base_tree": base_tree, "tree": [{
            "path": "xoa-hl-image-build.json", "mode": "100644", "type": "blob",
            "content": format!("{}\n", serde_json::to_string_pretty(build)?)
        }]}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tree_sha = sha(&tree)?;
    let at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let author = json!({"name": "XCP-HL image builder", "email": "xcp-hl-build@users.noreply.github.com", "date": at});
    let commit: Value = client.post(format!("{repo_api}/git/commits"))
        .json(&json!({
            "message": format!("build: provenance for {}", build["image_tag"].as_str().context("Missing image tag")?),
            "tree": tree_sha, "parents": [source_sha], "author": author, "committer": author
        }))
        .send().await?.error_for_status()?.json().await?;
    sha(&commit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn same_day_rebuilds_have_distinct_tags_and_keep_source_suffix() {
        let a = DateTime::parse_from_rfc3339("2026-10-28T01:02:03.000000001Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = DateTime::parse_from_rfc3339("2026-10-28T01:02:03.000000002Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_ne!(
            image_tag_at("abcdef012345", a),
            image_tag_at("abcdef012345", b)
        );
        assert_eq!(
            image_tag_at("abcdef012345", a),
            "xoa-image-20261028-010203000000001-abcdef0"
        );
    }

    #[tokio::test]
    async fn provenance_uses_exact_source_tree_parent_and_only_git_objects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!(
            "http://{}/repos/test/images",
            listener.local_addr().unwrap()
        );
        let source = "a".repeat(40);
        let build = manifest(
            "xoa-image-20261028-010203000000001-bbbbbbb",
            &"b".repeat(40),
            &source,
            "v1-ce2",
            "xoa-hl.rpm",
        );
        let server = tokio::spawn(async move {
            let replies = [
                json!({"tree":{"sha":"c".repeat(40)}}),
                json!({"sha":"d".repeat(40)}),
                json!({"sha":"e".repeat(40)}),
            ];
            let mut requests = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (end, length) = loop {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let length = header
                            .lines()
                            .find_map(|s| {
                                s.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break (end, length);
                        }
                    }
                };
                let header = String::from_utf8_lossy(&bytes[..end])
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned();
                let body = if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap()
                };
                requests.push((header, body));
                let body = reply.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
            }
            requests
        });
        let commit = create_provenance_commit(
            &reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            &api,
            &source,
            &build,
        )
        .await
        .unwrap();
        assert_eq!(commit, "e".repeat(40));
        let requests = server.await.unwrap();
        assert!(requests[0]
            .0
            .starts_with("GET /repos/test/images/git/commits/"));
        assert_eq!(requests[1].1["base_tree"], "c".repeat(40));
        let recorded: Value =
            serde_json::from_str(requests[1].1["tree"][0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(recorded, build);
        assert_eq!(requests[2].1["parents"], json!([source]));
        assert_eq!(requests[2].1["tree"], "d".repeat(40));
        assert!(!requests.iter().any(|r| r.0.contains("/refs")));
    }

    #[tokio::test]
    async fn failed_git_lookup_stops_before_creating_objects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            stream.read(&mut buf).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        assert!(create_provenance_commit(
            &reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            &api,
            &"a".repeat(40),
            &json!({})
        )
        .await
        .is_err());
        server.await.unwrap();
    }
}
