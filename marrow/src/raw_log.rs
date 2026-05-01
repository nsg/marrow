use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct RawLog {
    file: Arc<Mutex<Option<tokio::fs::File>>>,
}

impl RawLog {
    pub async fn new(path: Option<PathBuf>) -> std::io::Result<Self> {
        let file = if let Some(p) = path {
            if let Some(parent) = p.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .await?;
            Some(f)
        } else {
            None
        };

        Ok(Self {
            file: Arc::new(Mutex::new(file)),
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

    async fn write(&self, header: &str, body: &str) {
        let mut guard = self.file.lock().await;
        if let Some(f) = guard.as_mut() {
            let _ = f.write_all(header.as_bytes()).await;
            let _ = f.write_all(body.as_bytes()).await;
            let _ = f.write_all(b"\n\n").await;
        }
    }
}
