use std::path::Path;

use marrow::schedule::{self, Schedule};
use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct ScheduleInfo {
    pub id: String,
    pub description: String,
    pub repeat: String,
    pub enabled: bool,
    pub created: String,
    pub last_run: Option<String>,
    pub last_status: Option<String>,
    pub next_run: Option<String>,
    pub frontend: String,
}

pub fn load(dir: &Path) -> Vec<ScheduleInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut schedules = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(sched) = serde_json::from_str::<Schedule>(&content)
        {
            let repeat_str = format!("{:?}", sched.repeat);
            let next = schedule::next_run(&sched);
            schedules.push(ScheduleInfo {
                id: sched.id.to_string(),
                description: sched.description,
                repeat: repeat_str,
                enabled: sched.enabled,
                created: sched.created,
                last_run: sched.last_run,
                last_status: sched.last_status,
                next_run: next,
                frontend: sched.frontend,
            });
        }
    }
    schedules.sort_by(|a, b| b.created.cmp(&a.created));
    schedules
}
