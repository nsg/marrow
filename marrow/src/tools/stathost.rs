use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct StathostTool;

impl Tool for StathostTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "stathost".to_string(),
            description: "Manage files on a StatHost storage service. \
                 Actions: list (bucket contents), upload (CONTENT or LOCAL_FILE to REMOTE_PATH), \
                 delete (REMOTE_PATH). Requires BASE_URL, TOKEN, BUCKET"
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
    let root = upload_root()?;
    resolve_upload_source_from_root(local_file, &root).await
}

async fn resolve_upload_source_from_root(
    local_file: &str,
    root: &Path,
) -> Result<UploadSource, String> {
    let local_path = resolve_local_upload_path(local_file, root)?;

    let file_bytes = tokio::fs::read(&local_path)
        .await
        .map_err(|e| format!("failed to read file {}: {e}", local_path.display()))?;

    Ok(UploadSource { bytes: file_bytes })
}

fn upload_root() -> Result<PathBuf, String> {
    let current_dir =
        std::env::current_dir().map_err(|e| format!("failed to resolve current dir: {e}"))?;
    upload_root_in(&current_dir)
}

fn upload_root_in(base: &Path) -> Result<PathBuf, String> {
    let root = base.join("uploads");
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("failed to create upload root {}: {e}", root.display()))?;
    root.canonicalize()
        .map_err(|e| format!("failed to resolve upload root {}: {e}", root.display()))
}

fn resolve_local_upload_path(local_file: &str, root: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("failed to resolve upload root {}: {e}", root.display()))?;
    let requested = Path::new(local_file);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let path = candidate
        .canonicalize()
        .map_err(|e| format!("failed to resolve file {local_file}: {e}"))?;

    if !path.starts_with(&root) {
        return Err(format!(
            "LOCAL_FILE must be inside upload root {}",
            root.display()
        ));
    }
    if !path.is_file() {
        return Err(format!("LOCAL_FILE must be a file: {}", path.display()));
    }

    Ok(path)
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
        let dir = tempfile::tempdir().unwrap();
        let root = upload_root_in(dir.path()).unwrap();
        let path = root.join("upload.txt");
        std::fs::write(&path, b"test").unwrap();

        let upload = resolve_upload_source_from_root("upload.txt", &root)
            .await
            .unwrap();
        assert_eq!(upload.bytes, b"test");
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
        let dir = tempfile::tempdir().unwrap();
        let root = upload_root_in(dir.path()).unwrap();

        let error = resolve_upload_source_from_root("missing.txt", &root)
            .await
            .unwrap_err();
        assert!(error.starts_with("failed to resolve file missing.txt: "));
    }

    #[test]
    fn resolve_local_upload_path_rejects_paths_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();

        let error = resolve_local_upload_path(outside.path().to_str().unwrap(), root.path())
            .expect_err("outside path should be rejected");

        assert!(error.starts_with("LOCAL_FILE must be inside upload root "));
    }

    #[test]
    fn resolve_local_upload_path_rejects_directories() {
        let root = tempfile::tempdir().unwrap();
        let subdir = root.path().join("nested");
        std::fs::create_dir(&subdir).unwrap();

        let error = resolve_local_upload_path(subdir.to_str().unwrap(), root.path())
            .expect_err("directory path should be rejected");

        assert!(error.starts_with("LOCAL_FILE must be a file: "));
    }

    #[test]
    fn upload_root_creates_uploads_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = upload_root_in(dir.path()).unwrap();

        assert_eq!(root.file_name().and_then(|n| n.to_str()), Some("uploads"));
        assert!(root.is_dir());
    }
}
