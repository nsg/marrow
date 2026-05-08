use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct RawLog {
    file: Arc<Mutex<Option<tokio::fs::File>>>,
    error_file: Arc<Mutex<Option<tokio::fs::File>>>,
}

#[derive(Serialize)]
pub struct ErrorEntry {
    pub ts: String,
    pub role: String,
    pub url: String,
    pub error_type: String,
    pub status: u16,
    pub body: String,
}

async fn open_append(path: &PathBuf) -> std::io::Result<tokio::fs::File> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
}

impl RawLog {
    pub async fn new(path: Option<PathBuf>) -> std::io::Result<Self> {
        let file = match &path {
            Some(p) => Some(open_append(p).await?),
            None => None,
        };

        let error_file = match &path {
            Some(p) => {
                let mut err_path = p.clone();
                let stem = p
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                err_path.set_file_name(format!("{stem}.errors.jsonl"));
                Some(open_append(&err_path).await?)
            }
            None => None,
        };

        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            error_file: Arc::new(Mutex::new(error_file)),
        })
    }

    pub async fn log_request(&self, role: &str, url: &str, body: &str) {
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let header = format!(">>> REQUEST [{ts}] role={role} url={url}\n");
        self.write(&header, body).await;
    }

    pub async fn log_response(&self, role: &str, url: &str, body: &str) {
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let header = format!("<<< RESPONSE [{ts}] role={role} url={url}\n");
        self.write(&header, body).await;
    }

    pub async fn log_error(&self, role: &str, url: &str, status: u16, body: &str) {
        let error_type = if status == 0 { "network" } else { "http" };
        let entry = ErrorEntry {
            ts: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            role: role.to_string(),
            url: url.to_string(),
            error_type: error_type.to_string(),
            status,
            body: body.to_string(),
        };
        let mut guard = self.error_file.lock().await;
        if let Some(f) = guard.as_mut()
            && let Ok(json) = serde_json::to_string(&entry)
        {
            let _ = f.write_all(json.as_bytes()).await;
            let _ = f.write_all(b"\n").await;
        }
    }

    async fn write(&self, header: &str, body: &str) {
        let mut guard = self.file.lock().await;
        if let Some(f) = guard.as_mut() {
            let _ = f.write_all(header.as_bytes()).await;
            let _ = f.write_all(body.as_bytes()).await;
            let _ = f.write_all(b"\n\n").await;
        }
    }
}
