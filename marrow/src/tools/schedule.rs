use std::collections::HashMap;

use crate::schedule::{self, RepeatSpec, Schedule, WeekdaySpec};
use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

const MAX_INTERVAL_HOURS: u16 = 8_760;

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

            let action = match required_param(&params, "ACTION") {
                Ok(value) => value.to_lowercase(),
                Err(error) => return Ok(serde_json::json!({ "error": error })),
            };
            if !matches!(action.as_str(), "create" | "update") {
                return Ok(serde_json::json!({
                    "error": format!("invalid ACTION '{action}'. Use ACTION=create or ACTION=update")
                }));
            }
            let description = match params.get("DESCRIPTION") {
                Some(d) if !d.is_empty() => d.clone(),
                _ => {
                    return Ok(
                        serde_json::json!({"error": "missing required parameter: DESCRIPTION"}),
                    );
                }
            };

            let repeat_type = match required_param(&params, "REPEAT") {
                Ok(value) => value.to_lowercase(),
                Err(error) => return Ok(serde_json::json!({ "error": error })),
            };
            let tz_offset = match parse_timezone_offset(params.get("TIMEZONE_OFFSET")) {
                Ok(value) => value,
                Err(error) => return Ok(serde_json::json!({ "error": error })),
            };

            let repeat = match repeat_type.as_str() {
                "daily" => {
                    let (hour, minute) = match parse_hour_minute(&params, "daily") {
                        Ok(value) => value,
                        Err(error) => return Ok(serde_json::json!({ "error": error })),
                    };
                    RepeatSpec::Daily { hour, minute }
                }
                "every_n_hours" => {
                    let interval = match parse_interval(&params) {
                        Ok(value) => value,
                        Err(error) => return Ok(serde_json::json!({ "error": error })),
                    };
                    RepeatSpec::EveryNHours { interval }
                }
                "weekly" => {
                    let (hour, minute) = match parse_hour_minute(&params, "weekly") {
                        Ok(value) => value,
                        Err(error) => return Ok(serde_json::json!({ "error": error })),
                    };
                    let day = match params.get("DAY").and_then(|d| WeekdaySpec::parse(d)) {
                        Some(d) => d,
                        None => {
                            return Ok(
                                serde_json::json!({"error": "missing or invalid DAY for weekly schedule. Use DAY=monday, tuesday, wednesday, thursday, friday, saturday, or sunday"}),
                            );
                        }
                    };
                    RepeatSpec::Weekly { day, hour, minute }
                }
                "once" => {
                    let at = match parse_once_at(&params) {
                        Ok(value) => value,
                        Err(error) => return Ok(serde_json::json!({ "error": error })),
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
                _ => unreachable!("ACTION was validated before dispatch"),
            }
        })
    }
}

fn required_param(params: &HashMap<String, String>, name: &str) -> Result<String, String> {
    match params.get(name).map(|s| s.trim()) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        _ => Err(format!("missing required parameter: {name}")),
    }
}

fn parse_required_u8(
    params: &HashMap<String, String>,
    name: &str,
    min: u8,
    max: u8,
    context: &str,
) -> Result<u8, String> {
    let raw = required_param(params, name)?;
    let value: u8 = raw.parse().map_err(|_| {
        format!("invalid {name} '{raw}' for {context}. Use a whole number from {min} to {max}")
    })?;
    if !(min..=max).contains(&value) {
        return Err(format!(
            "invalid {name} '{raw}' for {context}. Use a whole number from {min} to {max}"
        ));
    }
    Ok(value)
}

fn parse_hour_minute(params: &HashMap<String, String>, context: &str) -> Result<(u8, u8), String> {
    Ok((
        parse_required_u8(params, "HOUR", 0, 23, context)?,
        parse_required_u8(params, "MINUTE", 0, 59, context)?,
    ))
}

fn parse_interval(params: &HashMap<String, String>) -> Result<u16, String> {
    let raw = required_param(params, "INTERVAL")?;
    let value: u16 = raw.parse().map_err(|_| {
        format!(
            "invalid INTERVAL '{raw}' for every_n_hours. Use a whole number from 1 to {MAX_INTERVAL_HOURS}"
        )
    })?;
    if !(1..=MAX_INTERVAL_HOURS).contains(&value) {
        return Err(format!(
            "invalid INTERVAL '{raw}' for every_n_hours. Use a whole number from 1 to {MAX_INTERVAL_HOURS}"
        ));
    }
    Ok(value)
}

fn parse_timezone_offset(raw: Option<&String>) -> Result<i32, String> {
    let Some(raw) = raw.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };
    let value: i32 = raw.parse().map_err(|_| {
        format!("invalid TIMEZONE_OFFSET '{raw}'. Use a whole-hour UTC offset from -12 to 14")
    })?;
    if !(-12..=14).contains(&value) {
        return Err(format!(
            "invalid TIMEZONE_OFFSET '{raw}'. Use a whole-hour UTC offset from -12 to 14"
        ));
    }
    Ok(value)
}

fn parse_once_at(params: &HashMap<String, String>) -> Result<String, String> {
    let raw = required_param(params, "AT")?;
    let parsed = chrono::DateTime::parse_from_rfc3339(&raw).or_else(|_| {
        chrono::DateTime::parse_from_rfc3339(&format!("{}+00:00", raw.trim_end_matches('Z')))
    });
    if parsed.is_err() {
        return Err(format!(
            "invalid AT '{raw}' for once schedule. Use an ISO 8601/RFC3339 datetime like 2026-04-29T09:00:00Z"
        ));
    }
    Ok(raw)
}

fn parse_status_filter(raw: Option<&String>) -> Result<String, String> {
    let filter = raw
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "all".to_string());
    match filter.as_str() {
        "all" | "enabled" | "disabled" => Ok(filter),
        _ => Err(format!(
            "invalid STATUS_FILTER '{filter}'. Use STATUS_FILTER=all, enabled, or disabled"
        )),
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

            let filter = match parse_status_filter(params.get("STATUS_FILTER")) {
                Ok(value) => value,
                Err(error) => return Ok(serde_json::json!({ "error": error })),
            };

            let all = store.list()?;
            let filtered: Vec<&Schedule> = match filter.as_str() {
                "enabled" => all.iter().filter(|s| s.enabled).collect(),
                "disabled" => all.iter().filter(|s| !s.enabled).collect(),
                "all" => all.iter().collect(),
                _ => unreachable!("parse_status_filter only returns known filters"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn params(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn parse_hour_minute_requires_valid_ranges() {
        let valid = params(&[("HOUR", "23"), ("MINUTE", "59")]);
        assert_eq!(parse_hour_minute(&valid, "daily").unwrap(), (23, 59));

        let bad_hour = params(&[("HOUR", "24"), ("MINUTE", "0")]);
        assert_eq!(
            parse_hour_minute(&bad_hour, "daily").unwrap_err(),
            "invalid HOUR '24' for daily. Use a whole number from 0 to 23"
        );

        let bad_minute = params(&[("HOUR", "8"), ("MINUTE", "soon")]);
        assert_eq!(
            parse_hour_minute(&bad_minute, "weekly").unwrap_err(),
            "invalid MINUTE 'soon' for weekly. Use a whole number from 0 to 59"
        );
    }

    #[test]
    fn parse_interval_requires_positive_bounded_hours() {
        assert_eq!(parse_interval(&params(&[("INTERVAL", "1")])).unwrap(), 1);
        assert!(parse_interval(&params(&[("INTERVAL", "0")])).is_err());
        assert!(parse_interval(&params(&[("INTERVAL", "9000")])).is_err());
        assert!(parse_interval(&params(&[("INTERVAL", "hourly")])).is_err());
    }

    #[test]
    fn parse_timezone_offset_requires_real_world_whole_hours() {
        assert_eq!(parse_timezone_offset(None).unwrap(), 0);
        assert_eq!(parse_timezone_offset(Some(&"14".to_string())).unwrap(), 14);
        assert_eq!(
            parse_timezone_offset(Some(&"-12".to_string())).unwrap(),
            -12
        );
        assert!(parse_timezone_offset(Some(&"15".to_string())).is_err());
        assert!(parse_timezone_offset(Some(&"1.5".to_string())).is_err());
    }

    #[test]
    fn parse_once_at_requires_rfc3339_like_datetime() {
        assert_eq!(
            parse_once_at(&params(&[("AT", "2026-04-29T09:00:00Z")])).unwrap(),
            "2026-04-29T09:00:00Z"
        );
        assert!(parse_once_at(&params(&[("AT", "tomorrow morning")])).is_err());
    }

    #[test]
    fn parse_status_filter_rejects_unknown_values() {
        assert_eq!(parse_status_filter(None).unwrap(), "all");
        assert_eq!(
            parse_status_filter(Some(&"enabled".to_string())).unwrap(),
            "enabled"
        );
        assert_eq!(
            parse_status_filter(Some(&"DISABLED".to_string())).unwrap(),
            "disabled"
        );
        assert_eq!(
            parse_status_filter(Some(&"paused".to_string())).unwrap_err(),
            "invalid STATUS_FILTER 'paused'. Use STATUS_FILTER=all, enabled, or disabled"
        );
    }
}
