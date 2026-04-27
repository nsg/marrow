use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use marrow::events::{Event, LogEntry};
use serde::Serialize;

#[derive(Default)]
pub struct EventData {
    pub entries: Vec<LogEntry>,
    byte_offset: u64,
}

#[derive(Serialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub description: String,
    pub role: String,
    pub status: Option<String>,
    pub timestamp_ms: u64,
    pub steps: u32,
    pub total_duration_ms: u64,
}

#[derive(Serialize)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub summary: TaskSummary,
    pub events: Vec<TaskEvent>,
}

#[derive(Serialize)]
pub struct TaskEvent {
    pub timestamp_ms: u64,
    #[serde(flatten)]
    pub event: serde_json::Value,
}

#[derive(Serialize)]
pub struct OverviewStats {
    pub total_tasks: usize,
    pub total_steps: usize,
    pub total_tool_calls: usize,
    pub total_code_runs: usize,
    pub success_rate: f64,
    pub activity_buckets: Vec<ActivityBucket>,
    pub event_timespan: Option<(u64, u64)>,
}

#[derive(Serialize)]
pub struct ActivityBucket {
    pub hour_ms: u64,
    pub count: usize,
}

impl EventData {
    pub fn load(path: &Path) -> Self {
        let mut data = Self::default();
        let Ok(file) = std::fs::File::open(path) else {
            return data;
        };
        let reader = BufReader::new(file);
        let mut offset = 0u64;

        for line in reader.lines() {
            let Ok(line) = line else { break };
            offset += line.len() as u64 + 1; // +1 for newline
            if let Ok(entry) = serde_json::from_str::<LogEntry>(&line) {
                data.entries.push(entry);
            }
        }

        data.byte_offset = offset;
        data
    }

    pub fn refresh(&mut self, path: &Path) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        let Ok(metadata) = file.metadata() else {
            return;
        };

        let file_len = metadata.len();
        if file_len < self.byte_offset {
            // File was truncated — full reload
            *self = Self::load(path);
            return;
        }
        if file_len == self.byte_offset {
            return; // No new data
        }

        if file.seek(SeekFrom::Start(self.byte_offset)).is_err() {
            return;
        }

        let reader = BufReader::new(file);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            self.byte_offset += line.len() as u64 + 1;
            if let Ok(entry) = serde_json::from_str::<LogEntry>(&line) {
                self.entries.push(entry);
            }
        }
    }

    pub fn overview(&self) -> OverviewStats {
        let mut total_tasks = 0usize;
        let mut total_steps = 0usize;
        let mut total_tool_calls = 0usize;
        let mut total_code_runs = 0usize;
        let mut step_successes = 0usize;
        let mut step_total = 0usize;

        for entry in &self.entries {
            match &entry.event {
                Event::TaskCreated { .. } => total_tasks += 1,
                Event::StepCompleted {
                    action_type,
                    success,
                    ..
                } => {
                    total_steps += 1;
                    step_total += 1;
                    if *success {
                        step_successes += 1;
                    }
                    if action_type.contains("call_tool") {
                        total_tool_calls += 1;
                    } else if action_type.contains("run_code") {
                        total_code_runs += 1;
                    }
                }
                Event::AgentAction { action_type, .. } => {
                    if action_type == "call_tool" {
                        total_tool_calls += 1;
                    } else if action_type == "run_code" {
                        total_code_runs += 1;
                    }
                }
                _ => {}
            }
        }

        // If step_completed events exist, use those for tool/code counts instead
        // (they're more accurate). Otherwise fall back to agent_action counts.
        if total_steps > 0 {
            // Recount from step_completed only
            total_tool_calls = 0;
            total_code_runs = 0;
            for entry in &self.entries {
                if let Event::StepCompleted { action_type, .. } = &entry.event {
                    if action_type.contains("call_tool") {
                        total_tool_calls += 1;
                    }
                    if action_type.contains("run_code") {
                        total_code_runs += 1;
                    }
                }
            }
        }

        let success_rate = if step_total > 0 {
            step_successes as f64 / step_total as f64
        } else {
            1.0
        };

        // Activity buckets: events per hour for last 48h
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let bucket_duration_ms = 3_600_000u64; // 1 hour
        let window_start = now_ms.saturating_sub(48 * bucket_duration_ms);

        let mut buckets: HashMap<u64, usize> = HashMap::new();
        for entry in &self.entries {
            if entry.timestamp_ms >= window_start {
                let bucket = (entry.timestamp_ms - window_start) / bucket_duration_ms;
                *buckets.entry(bucket).or_default() += 1;
            }
        }

        let activity_buckets: Vec<ActivityBucket> = (0..48)
            .map(|i| {
                let hour_ms = window_start + i * bucket_duration_ms;
                let count = buckets.get(&i).copied().unwrap_or(0);
                ActivityBucket { hour_ms, count }
            })
            .collect();

        let event_timespan = if self.entries.is_empty() {
            None
        } else {
            Some((
                self.entries.first().unwrap().timestamp_ms,
                self.entries.last().unwrap().timestamp_ms,
            ))
        };

        OverviewStats {
            total_tasks,
            total_steps,
            total_tool_calls,
            total_code_runs,
            success_rate,
            activity_buckets,
            event_timespan,
        }
    }

    pub fn tasks(&self, limit: usize, offset: usize) -> (Vec<TaskSummary>, usize) {
        let mut tasks: HashMap<String, TaskSummary> = HashMap::new();
        let mut task_order: Vec<String> = Vec::new();

        for entry in &self.entries {
            match &entry.event {
                Event::TaskCreated {
                    task_id,
                    description,
                    role,
                } => {
                    task_order.push(task_id.clone());
                    tasks.insert(
                        task_id.clone(),
                        TaskSummary {
                            task_id: task_id.clone(),
                            description: description.clone(),
                            role: role.clone(),
                            status: None,
                            timestamp_ms: entry.timestamp_ms,
                            steps: 0,
                            total_duration_ms: 0,
                        },
                    );
                }
                Event::TaskExecuted { task_id, status } => {
                    if let Some(t) = tasks.get_mut(task_id) {
                        t.status = Some(status.clone());
                    }
                }
                Event::StepCompleted {
                    task_id,
                    duration_ms,
                    ..
                } => {
                    if let Some(t) = tasks.get_mut(task_id) {
                        t.steps += 1;
                        t.total_duration_ms += duration_ms;
                    }
                }
                _ => {}
            }
        }

        // Reverse for newest first
        task_order.reverse();
        let total = task_order.len();
        let page: Vec<TaskSummary> = task_order
            .into_iter()
            .skip(offset)
            .take(limit)
            .filter_map(|id| tasks.remove(&id))
            .collect();

        (page, total)
    }

    pub fn task_detail(&self, target_id: &str) -> Option<TaskDetail> {
        let mut summary: Option<TaskSummary> = None;
        let mut events: Vec<TaskEvent> = Vec::new();

        for entry in &self.entries {
            let task_id = match &entry.event {
                Event::TaskCreated { task_id, .. }
                | Event::TaskExecuted { task_id, .. }
                | Event::ToolSelected { task_id, .. }
                | Event::ContextAssembled { task_id, .. }
                | Event::AgentAction { task_id, .. }
                | Event::AgentToolResult { task_id, .. }
                | Event::AgentModelResponse { task_id, .. }
                | Event::StepCompleted { task_id, .. } => Some(task_id.as_str()),
                _ => None,
            };

            if task_id != Some(target_id) {
                continue;
            }

            if let Event::TaskCreated {
                task_id,
                description,
                role,
            } = &entry.event
            {
                summary = Some(TaskSummary {
                    task_id: task_id.clone(),
                    description: description.clone(),
                    role: role.clone(),
                    status: None,
                    timestamp_ms: entry.timestamp_ms,
                    steps: 0,
                    total_duration_ms: 0,
                });
            }

            if let Event::TaskExecuted { status, .. } = &entry.event
                && let Some(s) = &mut summary
            {
                s.status = Some(status.clone());
            }

            if let Event::StepCompleted { duration_ms, .. } = &entry.event
                && let Some(s) = &mut summary
            {
                s.steps += 1;
                s.total_duration_ms += duration_ms;
            }

            // Serialize the event to a generic JSON value for the frontend
            if let Ok(val) = serde_json::to_value(&entry.event) {
                events.push(TaskEvent {
                    timestamp_ms: entry.timestamp_ms,
                    event: val,
                });
            }
        }

        summary.map(|s| TaskDetail { summary: s, events })
    }

    pub fn events_page(
        &self,
        limit: usize,
        offset: usize,
        type_filter: Option<&str>,
    ) -> (Vec<serde_json::Value>, usize) {
        let filtered: Vec<&LogEntry> = self
            .entries
            .iter()
            .rev()
            .filter(|e| {
                let Some(filter) = type_filter else {
                    return true;
                };
                event_category(&e.event) == filter
            })
            .collect();

        let total = filtered.len();
        let page: Vec<serde_json::Value> = filtered
            .into_iter()
            .skip(offset)
            .take(limit)
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect();

        (page, total)
    }

    pub fn janitor_history(&self) -> Vec<serde_json::Value> {
        self.entries
            .iter()
            .rev()
            .filter(|e| {
                matches!(
                    e.event,
                    Event::JanitorReview { .. }
                        | Event::JanitorRegenerate { .. }
                        | Event::JanitorEscalated { .. }
                        | Event::JanitorDeleted { .. }
                        | Event::ToolGenerated { .. }
                )
            })
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect()
    }
}

fn event_category(event: &Event) -> &'static str {
    match event {
        Event::TaskCreated { .. } | Event::TaskExecuted { .. } => "task",
        Event::ToolSelected { .. } | Event::ToolGenerated { .. } => "tool",
        Event::ContextAssembled { .. } => "task",
        Event::JanitorReview { .. }
        | Event::JanitorRegenerate { .. }
        | Event::JanitorEscalated { .. }
        | Event::JanitorDeleted { .. } => "janitor",
        Event::MemoryCleanupStarted { .. }
        | Event::MemoryCleanupResult { .. }
        | Event::SkillsGenerated { .. } => "janitor",
        Event::AgentAction { .. }
        | Event::AgentToolResult { .. }
        | Event::AgentModelResponse { .. }
        | Event::StepCompleted { .. } => "agent",
        Event::ScheduleCreated { .. }
        | Event::ScheduleDeleted { .. }
        | Event::ScheduleTriggered { .. }
        | Event::ScheduleCompleted { .. } => "schedule",
    }
}
