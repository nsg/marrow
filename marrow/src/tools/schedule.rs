use std::collections::HashMap;

use crate::schedule::{self, RepeatSpec, Schedule, WeekdaySpec};
use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

// ────────────────────────────────────────────────────────────────────────────
// schedule_task — create or update a scheduled task
// ────────────────────────────────────────────────────────────────────────────

pub struct ScheduleTaskTool;

impl Tool for ScheduleTaskTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "schedule_task".to_string(),
            description: "Create or update a scheduled task. Actions: create, update. \
                 REPEAT types: daily (HOUR+MINUTE), every_n_hours (INTERVAL in hours), \
                 weekly (DAY+HOUR+MINUTE), once (AT as ISO datetime). \
                 TIMEZONE_OFFSET: hours from UTC (e.g. 1 for CET, 2 for CEST)"
                .to_string(),
            provides: vec!["schedule_task".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::required("ACTION"),
            ParamDef::required("DESCRIPTION"),
            ParamDef::required("REPEAT"),
            ParamDef::optional("HOUR"),
            ParamDef::optional("MINUTE"),
            ParamDef::optional("INTERVAL"),
            ParamDef::optional("DAY"),
            ParamDef::optional("AT"),
            ParamDef::optional("TIMEZONE_OFFSET"),
            ParamDef::optional("SCHEDULE_ID"),
        ]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "schedule_id".to_string(),
            "description".to_string(),
            "repeat".to_string(),
            "next_run".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let store = match &ctx.schedule_store {
                Some(s) => s,
                None => {
                    return Ok(serde_json::json!({"error": "schedule store not available"}));
                }
            };

            let action = params
                .get("ACTION")
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            let description = match params.get("DESCRIPTION") {
                Some(d) if !d.is_empty() => d.clone(),
                _ => {
                    return Ok(
                        serde_json::json!({"error": "missing required parameter: DESCRIPTION"}),
                    );
                }
            };

            let repeat_type = params
                .get("REPEAT")
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            let hour: u8 = params.get("HOUR").and_then(|h| h.parse().ok()).unwrap_or(0);
            let minute: u8 = params
                .get("MINUTE")
                .and_then(|m| m.parse().ok())
                .unwrap_or(0);
            let tz_offset: i32 = params
                .get("TIMEZONE_OFFSET")
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);

            let repeat = match repeat_type.as_str() {
                "daily" => RepeatSpec::Daily { hour, minute },
                "every_n_hours" => {
                    let interval: u16 = params
                        .get("INTERVAL")
                        .and_then(|i| i.parse().ok())
                        .unwrap_or(1);
                    RepeatSpec::EveryNHours { interval }
                }
                "weekly" => {
                    let day = match params.get("DAY").and_then(|d| WeekdaySpec::parse(d)) {
                        Some(d) => d,
                        None => {
                            return Ok(
                                serde_json::json!({"error": "missing or invalid DAY parameter for weekly schedule (e.g., monday, tue)"}),
                            );
                        }
                    };
                    RepeatSpec::Weekly { day, hour, minute }
                }
                "once" => {
                    let at = match params.get("AT") {
                        Some(a) if !a.is_empty() => a.clone(),
                        _ => {
                            return Ok(
                                serde_json::json!({"error": "missing AT parameter for one-time schedule (ISO 8601 datetime)"}),
                            );
                        }
                    };
                    RepeatSpec::Once { at }
                }
                other => {
                    return Ok(serde_json::json!({
                        "error": format!("unknown REPEAT type: '{other}'. Use: daily, every_n_hours, weekly, once")
                    }));
                }
            };

            let frontend = ctx
                .frontend_context
                .as_ref()
                .map(|fc| fc.frontend.clone())
                .unwrap_or_else(|| "cli".to_string());
            let channel_id = ctx.frontend_context.as_ref().and_then(|fc| fc.channel_id);

            match action.as_str() {
                "create" => {
                    let sched =
                        Schedule::new(description, repeat, &frontend, channel_id, tz_offset);
                    store.save(&sched)?;
                    let next = schedule::next_run(&sched).unwrap_or_else(|| "unknown".to_string());
                    Ok(serde_json::json!({
                        "schedule_id": sched.id.to_string(),
                        "description": sched.description,
                        "repeat": sched.repeat.display(),
                        "next_run": next,
                    }))
                }
                "update" => {
                    let id_str = match params.get("SCHEDULE_ID") {
                        Some(id) if !id.is_empty() => id,
                        _ => {
                            return Ok(
                                serde_json::json!({"error": "missing SCHEDULE_ID for update action"}),
                            );
                        }
                    };
                    let id: uuid::Uuid = match id_str.parse() {
                        Ok(id) => id,
                        Err(_) => {
                            return Ok(
                                serde_json::json!({"error": format!("invalid SCHEDULE_ID: {id_str}")}),
                            );
                        }
                    };
                    let mut sched = match store.load(id) {
                        Ok(s) => s,
                        Err(_) => {
                            return Ok(
                                serde_json::json!({"error": format!("schedule not found: {id}")}),
                            );
                        }
                    };
                    sched.description = description;
                    sched.repeat = repeat;
                    sched.timezone_offset_hours = tz_offset;
                    store.update(&sched)?;
                    let next = schedule::next_run(&sched).unwrap_or_else(|| "unknown".to_string());
                    Ok(serde_json::json!({
                        "schedule_id": sched.id.to_string(),
                        "description": sched.description,
                        "repeat": sched.repeat.display(),
                        "next_run": next,
                    }))
                }
                other => Ok(serde_json::json!({
                    "error": format!("unknown ACTION: '{other}'. Use: create, update")
                })),
            }
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// list_schedules — list all scheduled tasks
// ────────────────────────────────────────────────────────────────────────────

pub struct ListSchedulesTool;

impl Tool for ListSchedulesTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "list_schedules".to_string(),
            description: "List all scheduled tasks with status and next run time. \
                 STATUS_FILTER: enabled, disabled, or omit for all"
                .to_string(),
            provides: vec!["list_schedules".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![ParamDef::optional("STATUS_FILTER")]
    }

    fn returns(&self) -> Vec<String> {
        vec!["schedules".to_string(), "count".to_string()]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let store = match &ctx.schedule_store {
                Some(s) => s,
                None => {
                    return Ok(serde_json::json!({"error": "schedule store not available"}));
                }
            };

            let filter = params
                .get("STATUS_FILTER")
                .map(|s| s.to_lowercase())
                .unwrap_or_else(|| "all".to_string());

            let all = store.list()?;
            let filtered: Vec<&Schedule> = match filter.as_str() {
                "enabled" => all.iter().filter(|s| s.enabled).collect(),
                "disabled" => all.iter().filter(|s| !s.enabled).collect(),
                _ => all.iter().collect(),
            };

            let schedules: Vec<serde_json::Value> = filtered
                .iter()
                .map(|s| {
                    let next = schedule::next_run(s).unwrap_or_else(|| "N/A".to_string());
                    serde_json::json!({
                        "id": s.id.to_string(),
                        "description": s.description,
                        "repeat": s.repeat.display(),
                        "enabled": s.enabled,
                        "last_run": s.last_run,
                        "last_status": s.last_status,
                        "next_run": next,
                        "frontend": s.frontend,
                    })
                })
                .collect();

            let count = schedules.len();
            Ok(serde_json::json!({
                "schedules": schedules,
                "count": count,
            }))
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// remove_schedule — delete, disable, or re-enable a schedule
// ────────────────────────────────────────────────────────────────────────────

pub struct RemoveScheduleTool;

impl Tool for RemoveScheduleTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "remove_schedule".to_string(),
            description: "Manage a scheduled task by ID. Actions: delete (permanent), disable (pause), enable (resume)"
                .to_string(),
            provides: vec!["remove_schedule".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::required("ACTION"),
            ParamDef::required("SCHEDULE_ID"),
        ]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "schedule_id".to_string(),
            "action".to_string(),
            "success".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let store = match &ctx.schedule_store {
                Some(s) => s,
                None => {
                    return Ok(serde_json::json!({"error": "schedule store not available"}));
                }
            };

            let action = params
                .get("ACTION")
                .map(|s| s.to_lowercase())
                .unwrap_or_default();

            let id_str = match params.get("SCHEDULE_ID") {
                Some(id) if !id.is_empty() => id,
                _ => {
                    return Ok(
                        serde_json::json!({"error": "missing required parameter: SCHEDULE_ID"}),
                    );
                }
            };

            let id: uuid::Uuid = match id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    return Ok(
                        serde_json::json!({"error": format!("invalid SCHEDULE_ID: {id_str}")}),
                    );
                }
            };

            match action.as_str() {
                "delete" => {
                    store.delete(id)?;
                    Ok(serde_json::json!({
                        "schedule_id": id.to_string(),
                        "action": "deleted",
                        "success": true,
                    }))
                }
                "disable" => {
                    let mut sched = match store.load(id) {
                        Ok(s) => s,
                        Err(_) => {
                            return Ok(
                                serde_json::json!({"error": format!("schedule not found: {id}")}),
                            );
                        }
                    };
                    sched.enabled = false;
                    store.update(&sched)?;
                    Ok(serde_json::json!({
                        "schedule_id": id.to_string(),
                        "action": "disabled",
                        "success": true,
                    }))
                }
                "enable" => {
                    let mut sched = match store.load(id) {
                        Ok(s) => s,
                        Err(_) => {
                            return Ok(
                                serde_json::json!({"error": format!("schedule not found: {id}")}),
                            );
                        }
                    };
                    sched.enabled = true;
                    store.update(&sched)?;
                    Ok(serde_json::json!({
                        "schedule_id": id.to_string(),
                        "action": "enabled",
                        "success": true,
                    }))
                }
                other => Ok(serde_json::json!({
                    "error": format!("unknown ACTION: '{other}'. Use: delete, disable, enable")
                })),
            }
        })
    }
}
