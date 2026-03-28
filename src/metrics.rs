use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct RoleMetrics {
    pub calls: u32,
    pub total_duration: Duration,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Default)]
pub struct Metrics {
    roles: Mutex<HashMap<String, RoleMetrics>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(
        &self,
        role: &str,
        duration: Duration,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        let mut roles = self.roles.lock().unwrap();
        let entry = roles.entry(role.to_string()).or_default();
        entry.calls += 1;
        entry.total_duration += duration;
        entry.prompt_tokens += prompt_tokens;
        entry.completion_tokens += completion_tokens;
    }

    pub fn summary(&self) -> Vec<(String, RoleMetrics)> {
        let roles = self.roles.lock().unwrap();
        let mut entries: Vec<_> = roles
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by_key(|(k, _)| k.clone());
        entries
    }

    pub fn display(&self) {
        let entries = self.summary();
        if entries.is_empty() {
            return;
        }

        eprintln!("\n--- Performance Metrics ---");
        let mut total_duration = Duration::ZERO;
        let mut total_calls = 0u32;
        let mut total_prompt = 0u64;
        let mut total_completion = 0u64;

        for (role, m) in &entries {
            let secs = m.total_duration.as_secs_f64();
            eprintln!(
                "[{role}] {calls} calls, {secs:.1}s total, {prompt} prompt tokens, {completion} completion tokens",
                calls = m.calls,
                prompt = m.prompt_tokens,
                completion = m.completion_tokens,
            );
            total_duration += m.total_duration;
            total_calls += m.calls;
            total_prompt += m.prompt_tokens;
            total_completion += m.completion_tokens;
        }

        let total_secs = total_duration.as_secs_f64();
        eprintln!(
            "[total] {total_calls} calls, {total_secs:.1}s, {total_prompt} prompt tokens, {total_completion} completion tokens"
        );
        eprintln!("---");
    }
}
