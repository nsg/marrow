use std::collections::HashMap;

use serde_json::json;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct StathostTool;

impl Tool for StathostTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "stathost".to_string(),
            description: "Manage files on a StatHost-compatible storage service — list bucket contents, upload files, and delete files"
                .to_string(),
            provides: vec!["stathost".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::required("ACTION"),
            ParamDef::required("BASE_URL"),
            ParamDef::required("TOKEN"),
            ParamDef::required("BUCKET"),
            ParamDef::optional("LOCAL_FILE"),
            ParamDef::optional("REMOTE_PATH"),
        ]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "action".to_string(),
            "status".to_string(),
            "body".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let action = match params.get("ACTION") {
                Some(a) if !a.is_empty() => a.as_str(),
                _ => {
                    return Ok(
                        json!({"error": "missing required parameter: ACTION (list, upload, delete)"}),
                    );
                }
            };
            let base_url = match params.get("BASE_URL") {
                Some(url) if !url.is_empty() => sanitize_base_url(url),
                _ => return Ok(json!({"error": "missing required parameter: BASE_URL"})),
            };
            let token = match params.get("TOKEN") {
                Some(t) if !t.is_empty() => t,
                _ => return Ok(json!({"error": "missing required parameter: TOKEN"})),
            };
            let bucket = match params.get("BUCKET") {
                Some(b) if !b.is_empty() => sanitize_bucket(b),
                _ => return Ok(json!({"error": "missing required parameter: BUCKET"})),
            };

            match action {
                "list" => list_bucket(&ctx, base_url, token, bucket).await,
                "upload" => {
                    let local_file = match params.get("LOCAL_FILE") {
                        Some(f) if !f.is_empty() => f.as_str(),
                        _ => return Ok(json!({"error": "upload requires LOCAL_FILE parameter"})),
                    };
                    let remote_path = match params.get("REMOTE_PATH") {
                        Some(p) if !p.is_empty() => sanitize_remote_path(p),
                        _ => return Ok(json!({"error": "upload requires REMOTE_PATH parameter"})),
                    };
                    upload_file(&ctx, base_url, token, bucket, local_file, remote_path).await
                }
                "delete" => {
                    let remote_path = match params.get("REMOTE_PATH") {
                        Some(p) if !p.is_empty() => sanitize_remote_path(p),
                        _ => return Ok(json!({"error": "delete requires REMOTE_PATH parameter"})),
                    };
                    delete_file(&ctx, base_url, token, bucket, remote_path).await
                }
                other => Ok(
                    json!({"error": format!("unknown action: {other}. Use: list, upload, delete")}),
                ),
            }
        })
    }
}

async fn list_bucket(
    ctx: &ToolContext,
    base_url: &str,
    token: &str,
    bucket: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{base_url}/{bucket}/_meta/list");

    let resp = match ctx.client.get(&url).bearer_auth(token).send().await {
        Ok(r) => r,
        Err(e) => return Ok(json!({"error": format!("list request failed: {e}")})),
    };

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    if status >= 400 {
        return Ok(json!({"error": format!("list failed: {status}"), "body": body}));
    }

    Ok(json!({
        "action": "list",
        "status": status,
        "body": body,
    }))
}

async fn upload_file(
    ctx: &ToolContext,
    base_url: &str,
    token: &str,
    bucket: &str,
    local_file: &str,
    remote_path: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let file_bytes = match tokio::fs::read(local_file).await {
        Ok(b) => b,
        Err(e) => {
            return Ok(json!({"error": format!("failed to read file {local_file}: {e}")}));
        }
    };

    let url = format!("{base_url}/{bucket}/{remote_path}");

    let resp = match ctx
        .client
        .put(&url)
        .bearer_auth(token)
        .body(file_bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(json!({"error": format!("upload request failed: {e}")})),
    };

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    if status >= 400 {
        return Ok(json!({"error": format!("upload failed: {status}"), "body": body}));
    }

    Ok(json!({
        "action": "upload",
        "status": status,
        "body": if body.trim().is_empty() { format!("{status} OK") } else { body },
    }))
}

async fn delete_file(
    ctx: &ToolContext,
    base_url: &str,
    token: &str,
    bucket: &str,
    remote_path: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{base_url}/{bucket}/{remote_path}");

    let resp = match ctx.client.delete(&url).bearer_auth(token).send().await {
        Ok(r) => r,
        Err(e) => return Ok(json!({"error": format!("delete request failed: {e}")})),
    };

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    if status >= 400 {
        return Ok(json!({"error": format!("delete failed: {status}"), "body": body}));
    }

    Ok(json!({
        "action": "delete",
        "status": status,
        "body": if body.trim().is_empty() { format!("{status} OK") } else { body },
    }))
}

fn sanitize_bucket(bucket: &str) -> &str {
    bucket.trim_matches('/')
}

fn sanitize_base_url(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

fn sanitize_remote_path(remote_path: &str) -> &str {
    remote_path.trim_start_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_base_url_removes_trailing_slashes() {
        assert_eq!(
            sanitize_base_url("https://example.com"),
            "https://example.com"
        );
        assert_eq!(
            sanitize_base_url("https://example.com/"),
            "https://example.com"
        );
        assert_eq!(
            sanitize_base_url("https://example.com///"),
            "https://example.com"
        );
    }

    #[test]
    fn sanitize_bucket_removes_outer_slashes() {
        assert_eq!(sanitize_bucket("bucket"), "bucket");
        assert_eq!(sanitize_bucket("/bucket/"), "bucket");
    }

    #[test]
    fn sanitize_remote_path_removes_leading_slashes() {
        assert_eq!(sanitize_remote_path("path/file.txt"), "path/file.txt");
        assert_eq!(sanitize_remote_path("/path/file.txt"), "path/file.txt");
    }
}
