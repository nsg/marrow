use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct RoleMetrics {
    pub calls: u32,
    pub total_duration: Duration,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's prompt cache (0 when the
    /// provider does not report this or caching is not available).
    pub cached_tokens: u64,
}

#[derive(Debug, Default)]
pub struct Metrics {
    roles: Mutex<HashMap<String, RoleMetrics>>,
}

tokio::task_local! {
    /// Per-task metrics instance set by `runtime::run_task()`.
    /// Backends automatically record into this via `Metrics::record()`.
    pub static TASK_METRICS: Arc<Metrics>;
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record metrics to this instance only (no task-local propagation).
    fn record_local(
        &self,
        role: &str,
        duration: Duration,
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
    ) {
        let mut roles = self.roles.lock().unwrap();
        let entry = roles.entry(role.to_string()).or_default();
        entry.calls += 1;
        entry.total_duration += duration;
        entry.prompt_tokens += prompt_tokens;
        entry.completion_tokens += completion_tokens;
        entry.cached_tokens += cached_tokens;
    }

    /// Record metrics to this instance and propagate to the per-task
    /// task-local (if one is set).
    pub fn record(
        &self,
        role: &str,
        duration: Duration,
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
    ) {
        self.record_local(
            role,
            duration,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
        );
        let _ = TASK_METRICS.try_with(|m| {
            m.record_local(
                role,
                duration,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
            );
        });
    }

    pub fn summary(&self) -> Vec<(String, RoleMetrics)> {
        let roles = self.roles.lock().unwrap();
        let mut entries: Vec<_> = roles.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
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
            let cached_info = if m.cached_tokens > 0 {
                format!(" ({} cached)", m.cached_tokens)
            } else {
                String::new()
            };
            eprintln!(
                "[{role}] {calls} calls, {secs:.1}s total, {prompt}{cached_info} prompt tokens, {completion} completion tokens",
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

/// Timing for a single agent loop step.
#[derive(Debug, Clone)]
pub struct StepTiming {
    pub step: u32,
    pub action: String,
    pub duration: Duration,
}

/// Per-task performance metrics collected during a single agent run.
#[derive(Debug, Clone, Default)]
pub struct TaskMetrics {
    /// Wall-clock time from task start to answer.
    pub wall_time: Duration,
    /// Number of agent loop iterations completed.
    pub steps: u32,
    /// Number of tool calls executed.
    pub tool_calls: u32,
    /// Number of inline code runs executed.
    pub code_runs: u32,
    /// Per-role model call statistics (timing + tokens).
    pub model_roles: Vec<(String, RoleMetrics)>,
    /// True when the agent exhausted the step limit and the answer was forced.
    pub hit_step_limit: bool,
    /// Per-step timing breakdown.
    pub step_timings: Vec<StepTiming>,
}

impl TaskMetrics {
    /// Compact one-line summary suitable for Discord subtext or log output.
    pub fn one_line(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("{:.1}s", self.wall_time.as_secs_f64()));
        if self.hit_step_limit {
            parts.push(format!("{} steps (limit)", self.steps));
        } else {
            parts.push(format!(
                "{} {}",
                self.steps,
                if self.steps == 1 { "step" } else { "steps" }
            ));
        }

        if !self.step_timings.is_empty() {
            let step_strs: Vec<String> = self
                .step_timings
                .iter()
                .map(|st| format!("{:.1}s", st.duration.as_secs_f64()))
                .collect();
            parts.push(format!("({})", step_strs.join(", ")));
        }

        if self.tool_calls > 0 {
            parts.push(format!(
                "{} {}",
                self.tool_calls,
                if self.tool_calls == 1 {
                    "tool"
                } else {
                    "tools"
                }
            ));
        }
        if self.code_runs > 0 {
            parts.push(format!("{} code", self.code_runs));
        }

        let mut total_tokens = 0u64;
        for (role, m) in &self.model_roles {
            parts.push(format!(
                "{role} {}x{:.1}s",
                m.calls,
                m.total_duration.as_secs_f64()
            ));
            total_tokens += m.prompt_tokens + m.completion_tokens;
        }

        if total_tokens > 0 {
            parts.push(format_tokens(total_tokens));
        }

        parts.join(" · ")
    }

    /// Verbose multi-line display for CLI stderr.
    pub fn display(&self) {
        eprintln!("\n--- Task Metrics ---");
        eprintln!("Wall time: {:.1}s", self.wall_time.as_secs_f64());
        eprintln!(
            "Steps: {}{}, Tool calls: {}, Code runs: {}",
            self.steps,
            if self.hit_step_limit {
                " (hit limit)"
            } else {
                ""
            },
            self.tool_calls,
            self.code_runs
        );
        for (role, m) in &self.model_roles {
            let cached_info = if m.cached_tokens > 0 {
                format!(" ({} cached)", m.cached_tokens)
            } else {
                String::new()
            };
            eprintln!(
                "[{role}] {} calls, {:.1}s, {}{cached_info} prompt / {} completion tokens",
                m.calls,
                m.total_duration.as_secs_f64(),
                m.prompt_tokens,
                m.completion_tokens
            );
        }
        if !self.step_timings.is_empty() {
            eprintln!("Step breakdown:");
            for st in &self.step_timings {
                eprintln!(
                    "  step {}: {} ({:.1}s)",
                    st.step,
                    st.action,
                    st.duration.as_secs_f64()
                );
            }
        }
        eprintln!("---");
    }
}

impl fmt::Display for TaskMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.one_line())
    }
}

/// Format a token count compactly (e.g. 1234 → "1.2k tokens").
fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M tokens", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k tokens", count as f64 / 1_000.0)
    } else {
        format!("{count} tokens")
    }
}
