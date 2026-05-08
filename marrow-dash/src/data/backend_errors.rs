use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct RawEntry {
    ts: String,
    role: String,
    url: String,
    error_type: String,
    status: u16,
    body: String,
}

#[derive(Serialize, Clone)]
pub struct ErrorRecord {
    pub ts: String,
    pub ts_ms: u64,
    pub role: String,
    pub url: String,
    pub error_type: String,
    pub status: u16,
    pub body: String,
}

#[derive(Serialize)]
pub struct StatusGroup {
    pub status: u16,
    pub count: usize,
    pub latest: String,
}

#[derive(Serialize)]
pub struct BodyGroup {
    pub status: u16,
    pub body: String,
    pub count: usize,
    pub latest: String,
}

#[derive(Serialize)]
pub struct RoleGroup {
    pub role: String,
    pub count: usize,
}

#[derive(Serialize)]
pub struct HourBucket {
    pub hour: String,
    pub count: usize,
}

#[derive(Serialize)]
pub struct BackendErrorSummary {
    pub total_errors: usize,
    pub by_status: Vec<StatusGroup>,
    pub by_body: Vec<BodyGroup>,
    pub by_role: Vec<RoleGroup>,
    pub hourly: Vec<HourBucket>,
}

#[derive(Serialize)]
pub struct BackendErrorsResponse {
    pub summary: BackendErrorSummary,
    pub errors: Vec<ErrorRecord>,
    pub total: usize,
}

#[derive(Default)]
pub struct BackendErrorData {
    entries: Vec<ErrorRecord>,
    byte_offset: u64,
}

fn parse_ts_ms(ts: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .or_else(|_| chrono::DateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.3fZ"))
        .map(|dt| dt.timestamp_millis() as u64)
        .unwrap_or(0)
}

fn truncate_body(body: &str, max: usize) -> String {
    if body.len() <= max {
        body.to_string()
    } else {
        format!("{}…", &body[..max])
    }
}

impl BackendErrorData {
    pub fn load(path: &Path) -> Self {
        let mut data = Self::default();
        data.read_from(path);
        data
    }

    pub fn refresh(&mut self, path: &Path) {
        self.read_from(path);
    }

    fn read_from(&mut self, path: &Path) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        let Ok(meta) = file.metadata() else {
            return;
        };

        if meta.len() < self.byte_offset {
            self.entries.clear();
            self.byte_offset = 0;
        }

        if meta.len() == self.byte_offset {
            return;
        }

        if self.byte_offset > 0 {
            let _ = file.seek(SeekFrom::Start(self.byte_offset));
        }

        let reader = BufReader::new(&file);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(raw): Result<RawEntry, _> = serde_json::from_str(&line) else {
                continue;
            };
            let ts_ms = parse_ts_ms(&raw.ts);
            self.entries.push(ErrorRecord {
                ts: raw.ts,
                ts_ms,
                role: raw.role,
                url: raw.url,
                error_type: raw.error_type,
                status: raw.status,
                body: raw.body,
            });
        }
        self.byte_offset = meta.len();
    }

    pub fn query(
        &self,
        limit: usize,
        offset: usize,
        status_filter: Option<u16>,
        role_filter: Option<&str>,
    ) -> BackendErrorsResponse {
        let filtered: Vec<&ErrorRecord> = self
            .entries
            .iter()
            .filter(|e| status_filter.is_none_or(|s| e.status == s))
            .filter(|e| role_filter.is_none_or(|r| e.role == r))
            .collect();

        let total = filtered.len();

        // By status
        let mut status_map: HashMap<u16, (usize, String)> = HashMap::new();
        for e in &filtered {
            let entry = status_map.entry(e.status).or_insert((0, String::new()));
            entry.0 += 1;
            if e.ts > entry.1 {
                entry.1 = e.ts.clone();
            }
        }
        let mut by_status: Vec<StatusGroup> = status_map
            .into_iter()
            .map(|(status, (count, latest))| StatusGroup {
                status,
                count,
                latest,
            })
            .collect();
        by_status.sort_by_key(|s| std::cmp::Reverse(s.count));

        // By body (status + truncated body)
        let mut body_map: HashMap<(u16, String), (usize, String)> = HashMap::new();
        for e in &filtered {
            let key = (e.status, truncate_body(&e.body, 200));
            let entry = body_map.entry(key).or_insert((0, String::new()));
            entry.0 += 1;
            if e.ts > entry.1 {
                entry.1 = e.ts.clone();
            }
        }
        let mut by_body: Vec<BodyGroup> = body_map
            .into_iter()
            .map(|((status, body), (count, latest))| BodyGroup {
                status,
                body,
                count,
                latest,
            })
            .collect();
        by_body.sort_by_key(|b| std::cmp::Reverse(b.count));

        // By role
        let mut role_map: HashMap<String, usize> = HashMap::new();
        for e in &filtered {
            *role_map.entry(e.role.clone()).or_default() += 1;
        }
        let mut by_role: Vec<RoleGroup> = role_map
            .into_iter()
            .map(|(role, count)| RoleGroup { role, count })
            .collect();
        by_role.sort_by_key(|r| std::cmp::Reverse(r.count));

        // Hourly buckets (last 48h)
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        let cutoff = now_ms.saturating_sub(48 * 3600 * 1000);
        let mut hour_map: HashMap<String, usize> = HashMap::new();
        for e in &filtered {
            if e.ts_ms >= cutoff {
                let hour = &e.ts[..13];
                *hour_map.entry(hour.to_string()).or_default() += 1;
            }
        }
        let mut hourly: Vec<HourBucket> = hour_map
            .into_iter()
            .map(|(hour, count)| HourBucket { hour, count })
            .collect();
        hourly.sort_by(|a, b| a.hour.cmp(&b.hour));

        // Paginated entries (newest first)
        let page: Vec<ErrorRecord> = filtered
            .iter()
            .rev()
            .skip(offset)
            .take(limit)
            .map(|e| ErrorRecord {
                body: truncate_body(&e.body, 500),
                ts: e.ts.clone(),
                ts_ms: e.ts_ms,
                role: e.role.clone(),
                url: e.url.clone(),
                error_type: e.error_type.clone(),
                status: e.status,
            })
            .collect();

        BackendErrorsResponse {
            summary: BackendErrorSummary {
                total_errors: total,
                by_status,
                by_body,
                by_role,
                hourly,
            },
            errors: page,
            total,
        }
    }
}
