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
            ParamDef::optional("CONTENT"),
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
                    let remote_path = match params.get("REMOTE_PATH") {
                        Some(p) if !p.is_empty() => sanitize_remote_path(p),
                        _ => return Ok(json!({"error": "upload requires REMOTE_PATH parameter"})),
                    };
                    let upload = match resolve_upload_source(&params).await {
                        Ok(upload) => upload,
                        Err(error) => return Ok(json!({"error": error})),
                    };
                    upload_file(&ctx, base_url, token, bucket, upload, remote_path).await
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
    upload: UploadSource,
    remote_path: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{base_url}/{bucket}/{remote_path}");

    let resp = match ctx
        .client
        .put(&url)
        .bearer_auth(token)
        .body(upload.bytes)
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

#[derive(Debug)]
struct UploadSource {
    bytes: Vec<u8>,
}

async fn resolve_upload_source(params: &HashMap<String, String>) -> Result<UploadSource, String> {
    if let Some(content) = params.get("CONTENT") {
        return Ok(UploadSource {
            bytes: content.as_bytes().to_vec(),
        });
    }

    let local_file = match params.get("LOCAL_FILE") {
        Some(f) if !f.is_empty() => f,
        _ => return Err("upload requires CONTENT or LOCAL_FILE parameter".to_string()),
    };

    let file_bytes = tokio::fs::read(local_file)
        .await
        .map_err(|e| format!("failed to read file {local_file}: {e}"))?;

    Ok(UploadSource { bytes: file_bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[tokio::test]
    async fn resolve_upload_source_prefers_content_argument() {
        let mut params = HashMap::new();
        params.insert("CONTENT".to_string(), "hello world".to_string());

        let upload = resolve_upload_source(&params).await.unwrap();

        assert_eq!(upload.bytes, b"hello world");
    }

    #[tokio::test]
    async fn resolve_upload_source_allows_empty_content() {
        let mut params = HashMap::new();
        params.insert("CONTENT".to_string(), String::new());
        params.insert(
            "LOCAL_FILE".to_string(),
            "/tmp/should-not-be-used-when-content-is-present".to_string(),
        );

        let upload = resolve_upload_source(&params).await.unwrap();

        assert!(upload.bytes.is_empty());
    }

    #[tokio::test]
    async fn resolve_upload_source_reads_local_file() {
        let path = std::env::temp_dir().join(format!(
            "marrow-stathost-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, b"test").unwrap();

        let mut params = HashMap::new();
        params.insert("LOCAL_FILE".to_string(), path.to_str().unwrap().to_string());

        let upload = resolve_upload_source(&params).await.unwrap();
        assert_eq!(upload.bytes, b"test");

        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn resolve_upload_source_rejects_missing_inputs() {
        let params = HashMap::new();

        assert_eq!(
            resolve_upload_source(&params).await.unwrap_err(),
            "upload requires CONTENT or LOCAL_FILE parameter"
        );
    }

    #[tokio::test]
    async fn resolve_upload_source_reports_missing_file() {
        let missing = format!(
            "/tmp/marrow-stathost-missing-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let mut params = HashMap::new();
        params.insert("LOCAL_FILE".to_string(), missing.clone());

        let error = resolve_upload_source(&params).await.unwrap_err();
        assert!(error.starts_with(&format!("failed to read file {missing}: ")));
    }
}
