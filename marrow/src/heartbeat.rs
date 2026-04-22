use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::events::{Event, EventLog};
use crate::memory::now_iso;
use crate::runtime::Runtime;
use crate::schedule::{self, RepeatSpec, ScheduleStore};
use crate::tool::FrontendContext;

/// Result of a scheduled task execution, delivered to the frontend.
pub struct ScheduleResult {
    pub schedule_id: Uuid,
    pub description: String,
    pub answer: String,
    pub frontend: String,
    pub channel_id: Option<u64>,
    pub success: bool,
}

/// Run the heartbeat loop.
///
/// Ticks every `tick_seconds`, checks for due schedules, and executes them
/// through the normal agent loop. Results are sent to `result_tx` for the
/// frontend to deliver.
///
/// Spawned by the frontend after constructing `Arc<Runtime>`.
pub async fn run(
    runtime: Arc<Runtime>,
    schedule_store: Arc<ScheduleStore>,
    log: Arc<EventLog>,
    result_tx: mpsc::UnboundedSender<ScheduleResult>,
    tick_seconds: u64,
) {
    let running: Arc<Mutex<HashSet<Uuid>>> = Arc::new(Mutex::new(HashSet::new()));

    loop {
        tokio::time::sleep(Duration::from_secs(tick_seconds)).await;

        let now = chrono::Utc::now();
        let schedules = match schedule_store.list_enabled() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[heartbeat] error listing schedules: {e}");
                continue;
            }
        };

        for sched in schedules {
            if !schedule::is_due(&sched, &now) {
                continue;
            }

            // Overlap protection
            {
                let active = running.lock().await;
                if active.contains(&sched.id) {
                    continue;
                }
            }

            // Mark as running
            {
                let mut active = running.lock().await;
                active.insert(sched.id);
            }

            let rt = runtime.clone();
            let store = schedule_store.clone();
            let log = log.clone();
            let tx = result_tx.clone();
            let running = running.clone();

            tokio::spawn(async move {
                execute_schedule(&sched, &rt, &store, &log, &tx).await;

                // Unmark as running
                let mut active = running.lock().await;
                active.remove(&sched.id);
            });
        }
    }
}

/// Run a single heartbeat pass: check all due schedules and execute them.
/// Returns the number of schedules executed.
pub async fn run_once(
    runtime: &Runtime,
    schedule_store: &ScheduleStore,
    log: &EventLog,
    result_tx: &mpsc::UnboundedSender<ScheduleResult>,
) -> u32 {
    let now = chrono::Utc::now();
    let schedules = match schedule_store.list_enabled() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[heartbeat] error listing schedules: {e}");
            return 0;
        }
    };

    let mut count = 0;
    for sched in &schedules {
        if schedule::is_due(sched, &now) {
            execute_schedule(sched, runtime, schedule_store, log, result_tx).await;
            count += 1;
        }
    }

    count
}

async fn execute_schedule(
    sched: &schedule::Schedule,
    runtime: &Runtime,
    store: &ScheduleStore,
    log: &EventLog,
    result_tx: &mpsc::UnboundedSender<ScheduleResult>,
) {
    log.emit(Event::ScheduleTriggered {
        schedule_id: sched.id.to_string(),
        description: sched.description.clone(),
    })
    .await;

    let frontend_ctx = FrontendContext {
        frontend: sched.frontend.clone(),
        channel_id: sched.channel_id,
    };

    let result = runtime
        .run_task(
            &sched.description,
            "scheduler",
            &[],
            None,
            None,
            None,
            Some(frontend_ctx),
        )
        .await;

    let (answer, success) = match result {
        Ok(answer) => (answer, true),
        Err(e) => (format!("Error: {e}"), false),
    };

    let status = if success { "succeeded" } else { "failed" };

    // Update schedule state
    let mut updated = sched.clone();
    updated.last_run = Some(now_iso());
    updated.last_status = Some(status.to_string());

    // Auto-disable one-shot schedules
    if matches!(sched.repeat, RepeatSpec::Once { .. }) {
        updated.enabled = false;
    }

    if let Err(e) = store.update(&updated) {
        eprintln!("[heartbeat] failed to update schedule {}: {e}", sched.id);
    }

    log.emit(Event::ScheduleCompleted {
        schedule_id: sched.id.to_string(),
        status: status.to_string(),
    })
    .await;

    let _ = result_tx.send(ScheduleResult {
        schedule_id: sched.id,
        description: sched.description.clone(),
        answer,
        frontend: sched.frontend.clone(),
        channel_id: sched.channel_id,
        success,
    });
}
