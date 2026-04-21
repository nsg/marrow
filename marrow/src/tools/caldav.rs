use std::collections::HashMap;

use reqwest::Client;
use reqwest::header::CONTENT_TYPE;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

// ---------------------------------------------------------------------------
// CalDAV Calendar Tool (VEVENT)
// ---------------------------------------------------------------------------

pub struct CalDavCalendarTool;

impl Tool for CalDavCalendarTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "caldav_calendar".to_string(),
            description:
                "Interacts with a CalDAV server to list calendars, fetch events, or create events"
                    .to_string(),
            provides: vec!["caldav_calendar".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::required("ACTION"),
            ParamDef::required("SERVER_URL"),
            ParamDef::required("USERNAME"),
            ParamDef::required("PASSWORD"),
            ParamDef::optional("CALENDAR_PATH"),
            ParamDef::optional("START_DATE"),
            ParamDef::optional("END_DATE"),
            ParamDef::optional("UID"),
            ParamDef::optional("SUMMARY"),
            ParamDef::optional("DESCRIPTION"),
        ]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "action".to_string(),
            "calendars".to_string(),
            "events".to_string(),
            "event".to_string(),
            "created_uid".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let action = match params.get("ACTION") {
                Some(a) if !a.is_empty() => a.to_lowercase(),
                _ => {
                    return Ok(serde_json::json!({
                        "error": "missing required parameter: ACTION (list_calendars, list_events, get_event, create_event)"
                    }));
                }
            };

            let server_url = params.get("SERVER_URL").map(|s| s.as_str()).unwrap_or("");
            if server_url.is_empty() {
                return Ok(serde_json::json!({
                    "error": "missing required parameter: SERVER_URL"
                }));
            }

            let auth = build_auth_from_params(&params);

            match action.as_str() {
                "list_calendars" => list_calendars(&ctx.client, server_url, &auth).await,
                "list_events" => {
                    let cal_path = require_param(&params, "CALENDAR_PATH", "list_events")?;
                    let start = params.get("START_DATE").cloned().unwrap_or_default();
                    let end = params.get("END_DATE").cloned().unwrap_or_default();
                    list_events(&ctx.client, server_url, &cal_path, &start, &end, &auth).await
                }
                "get_event" => {
                    let cal_path = require_param(&params, "CALENDAR_PATH", "get_event")?;
                    let uid = require_param(&params, "UID", "get_event")?;
                    get_event(&ctx.client, server_url, &cal_path, &uid, &auth).await
                }
                "create_event" => {
                    let cal_path = require_param(&params, "CALENDAR_PATH", "create_event")?;
                    let summary = require_param(&params, "SUMMARY", "create_event")?;
                    let start = require_param(&params, "START_DATE", "create_event")?;
                    let end = params.get("END_DATE").cloned().unwrap_or_default();
                    let description = params.get("DESCRIPTION").cloned().unwrap_or_default();
                    create_event(
                        &ctx.client,
                        server_url,
                        &cal_path,
                        &summary,
                        &start,
                        &end,
                        &description,
                        &auth,
                    )
                    .await
                }
                other => Ok(serde_json::json!({
                    "error": format!("unknown action: {other}. Use: list_calendars, list_events, get_event, create_event")
                })),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// CalDAV Tasks Tool (VTODO)
// ---------------------------------------------------------------------------

pub struct CalDavTasksTool;

impl Tool for CalDavTasksTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "caldav_tasks".to_string(),
            description:
                "Interacts with a CalDAV server to list, create, complete, or delete tasks (VTODO)"
                    .to_string(),
            provides: vec!["caldav_tasks".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::required("ACTION"),
            ParamDef::required("SERVER_URL"),
            ParamDef::required("USERNAME"),
            ParamDef::required("PASSWORD"),
            ParamDef::optional("CALENDAR_PATH"),
            ParamDef::optional("UID"),
            ParamDef::optional("SUMMARY"),
            ParamDef::optional("DESCRIPTION"),
            ParamDef::optional("DUE"),
            ParamDef::optional("PRIORITY"),
            ParamDef::optional("STATUS_FILTER"),
        ]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "action".to_string(),
            "tasks".to_string(),
            "task".to_string(),
            "created_uid".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let action = match params.get("ACTION") {
                Some(a) if !a.is_empty() => a.to_lowercase(),
                _ => {
                    return Ok(serde_json::json!({
                        "error": "missing required parameter: ACTION (list_tasks, create_task, complete_task, delete_task)"
                    }));
                }
            };

            let server_url = params.get("SERVER_URL").map(|s| s.as_str()).unwrap_or("");
            if server_url.is_empty() {
                return Ok(serde_json::json!({
                    "error": "missing required parameter: SERVER_URL"
                }));
            }

            let auth = build_auth_from_params(&params);

            match action.as_str() {
                "list_tasks" => {
                    let cal_path = require_param(&params, "CALENDAR_PATH", "list_tasks")?;
                    let status_filter = params.get("STATUS_FILTER").cloned().unwrap_or_default();
                    list_tasks(&ctx.client, server_url, &cal_path, &status_filter, &auth).await
                }
                "create_task" => {
                    let cal_path = require_param(&params, "CALENDAR_PATH", "create_task")?;
                    let summary = require_param(&params, "SUMMARY", "create_task")?;
                    let description = params.get("DESCRIPTION").cloned().unwrap_or_default();
                    let due = params.get("DUE").cloned().unwrap_or_default();
                    let priority = params.get("PRIORITY").cloned().unwrap_or_default();
                    create_task(
                        &ctx.client,
                        server_url,
                        &cal_path,
                        &summary,
                        &description,
                        &due,
                        &priority,
                        &auth,
                    )
                    .await
                }
                "complete_task" => {
                    let cal_path = require_param(&params, "CALENDAR_PATH", "complete_task")?;
                    let uid = require_param(&params, "UID", "complete_task")?;
                    complete_task(&ctx.client, server_url, &cal_path, &uid, &auth).await
                }
                "delete_task" => {
                    let cal_path = require_param(&params, "CALENDAR_PATH", "delete_task")?;
                    let uid = require_param(&params, "UID", "delete_task")?;
                    delete_task(&ctx.client, server_url, &cal_path, &uid, &auth).await
                }
                other => Ok(serde_json::json!({
                    "error": format!("unknown action: {other}. Use: list_tasks, create_task, complete_task, delete_task")
                })),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Shared CalDAV helpers
// ---------------------------------------------------------------------------

type ToolResult = Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;

fn require_param(
    params: &HashMap<String, String>,
    name: &str,
    action: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    match params.get(name) {
        Some(v) if !v.is_empty() => Ok(v.clone()),
        _ => Err(format!("{name} required for {action}").into()),
    }
}

fn build_auth_from_params(params: &HashMap<String, String>) -> Option<(String, String)> {
    let user = params.get("USERNAME").filter(|s| !s.is_empty())?;
    let pass = params.get("PASSWORD").map(|s| s.as_str()).unwrap_or("");
    Some((user.to_string(), pass.to_string()))
}

async fn caldav_request(
    client: &Client,
    method: &str,
    url: &str,
    body: &str,
    auth: &Option<(String, String)>,
) -> Result<(u16, String), Box<dyn std::error::Error + Send + Sync>> {
    let method_bytes = method.as_bytes();
    let req_method = reqwest::Method::from_bytes(method_bytes)
        .map_err(|e| format!("invalid method {method}: {e}"))?;

    let mut builder = client
        .request(req_method, url)
        .header(CONTENT_TYPE, "application/xml; charset=utf-8")
        .header("Depth", "1");

    if let Some((user, pass)) = auth {
        builder = builder.basic_auth(user, Some(pass));
    }

    if !body.is_empty() {
        builder = builder.body(body.to_string());
    }

    let resp = builder.send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await?;
    Ok((status, text))
}

fn resolve_url(server_url: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    let base = server_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    format!("{base}{path}")
}

/// Normalize ISO dates (2026-04-20T10:00:00) to iCal format (20260420T100000).
/// Passes through already-formatted iCal dates unchanged.
fn normalize_date(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    // If it already looks like iCal format (no dashes before T), pass through
    let before_t = input.split('T').next().unwrap_or(input);
    if !before_t.contains('-') {
        return input.to_string();
    }
    // Strip dashes and colons: 2026-04-20T10:00:00Z -> 20260420T100000Z
    input.replace(['-', ':'], "")
}

// ---------------------------------------------------------------------------
// Calendar operations
// ---------------------------------------------------------------------------

async fn list_calendars(
    client: &Client,
    server_url: &str,
    auth: &Option<(String, String)>,
) -> ToolResult {
    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cs="urn:ietf:params:xml:ns:caldav" xmlns:apple="http://apple.com/ns/ical/">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <cs:supported-calendar-component-set/>
    <apple:calendar-color/>
  </d:prop>
</d:propfind>"#;

    let (status, text) = caldav_request(client, "PROPFIND", server_url, body, auth).await?;

    if status >= 400 {
        return Ok(serde_json::json!({
            "error": format!("PROPFIND failed with status {status}"),
            "body": truncate(&text, 2000),
        }));
    }

    let calendars = parse_calendar_list(&text);

    Ok(serde_json::json!({
        "action": "list_calendars",
        "calendars": calendars,
    }))
}

async fn list_events(
    client: &Client,
    server_url: &str,
    cal_path: &str,
    start: &str,
    end: &str,
    auth: &Option<(String, String)>,
) -> ToolResult {
    let url = resolve_url(server_url, cal_path);

    let time_range = if !start.is_empty() || !end.is_empty() {
        let s = if start.is_empty() {
            "19700101T000000Z".to_string()
        } else {
            normalize_date(start)
        };
        let e = if end.is_empty() {
            "20991231T235959Z".to_string()
        } else {
            normalize_date(end)
        };
        format!(r#"<C:time-range start="{s}" end="{e}"/>"#)
    } else {
        String::new()
    };

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        {time_range}
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#
    );

    let (status, text) = caldav_request(client, "REPORT", &url, &body, auth).await?;

    if status >= 400 {
        return Ok(serde_json::json!({
            "error": format!("REPORT failed with status {status}"),
            "body": truncate(&text, 2000),
        }));
    }

    let events = parse_events_from_multistatus(&text);

    Ok(serde_json::json!({
        "action": "list_events",
        "calendar_path": cal_path,
        "event_count": events.len(),
        "events": events,
    }))
}

async fn get_event(
    client: &Client,
    server_url: &str,
    cal_path: &str,
    uid: &str,
    auth: &Option<(String, String)>,
) -> ToolResult {
    let url = resolve_url(server_url, cal_path);

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:prop-filter name="UID">
          <C:text-match>{uid}</C:text-match>
        </C:prop-filter>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#
    );

    let (status, text) = caldav_request(client, "REPORT", &url, &body, auth).await?;

    if status >= 400 {
        return Ok(serde_json::json!({
            "error": format!("REPORT failed with status {status}"),
            "body": truncate(&text, 2000),
        }));
    }

    let events = parse_events_from_multistatus(&text);
    let event = events.into_iter().next();

    Ok(serde_json::json!({
        "action": "get_event",
        "event": event,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn create_event(
    client: &Client,
    server_url: &str,
    cal_path: &str,
    summary: &str,
    start: &str,
    end: &str,
    description: &str,
    auth: &Option<(String, String)>,
) -> ToolResult {
    let uid = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let start_normalized = normalize_date(start);
    let end_normalized = if end.is_empty() {
        start_normalized.clone()
    } else {
        normalize_date(end)
    };

    // Determine if this is an all-day event (no T in the date) or timed
    let (dtstart, dtend) = if start_normalized.contains('T') {
        (
            format!("DTSTART:{start_normalized}"),
            format!("DTEND:{end_normalized}"),
        )
    } else {
        (
            format!("DTSTART;VALUE=DATE:{start_normalized}"),
            format!("DTEND;VALUE=DATE:{end_normalized}"),
        )
    };

    let mut vcal = format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//Marrow//CalDAV Tool//EN\r\n\
         BEGIN:VEVENT\r\n\
         UID:{uid}\r\n\
         DTSTAMP:{now}\r\n\
         {dtstart}\r\n\
         {dtend}\r\n\
         SUMMARY:{summary}\r\n"
    );

    if !description.is_empty() {
        vcal.push_str(&format!("DESCRIPTION:{description}\r\n"));
    }

    vcal.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");

    let resource_url = format!(
        "{}/{uid}.ics",
        resolve_url(server_url, cal_path).trim_end_matches('/')
    );

    let mut builder = client
        .put(&resource_url)
        .header(CONTENT_TYPE, "text/calendar; charset=utf-8")
        .header("If-None-Match", "*")
        .body(vcal);

    if let Some((user, pass)) = auth {
        builder = builder.basic_auth(user, Some(pass));
    }

    let resp = builder.send().await?;
    let status = resp.status().as_u16();

    if status >= 400 {
        let body = resp.text().await.unwrap_or_default();
        return Ok(serde_json::json!({
            "error": format!("PUT failed with status {status}"),
            "body": truncate(&body, 2000),
        }));
    }

    Ok(serde_json::json!({
        "action": "create_event",
        "created_uid": uid,
        "summary": summary,
    }))
}

// ---------------------------------------------------------------------------
// Tasks operations
// ---------------------------------------------------------------------------

async fn list_tasks(
    client: &Client,
    server_url: &str,
    cal_path: &str,
    status_filter: &str,
    auth: &Option<(String, String)>,
) -> ToolResult {
    let url = resolve_url(server_url, cal_path);

    let status_match = if !status_filter.is_empty() {
        format!(
            r#"<C:prop-filter name="STATUS">
          <C:text-match>{status_filter}</C:text-match>
        </C:prop-filter>"#
        )
    } else {
        String::new()
    };

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VTODO">
        {status_match}
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#
    );

    let (status, text) = caldav_request(client, "REPORT", &url, &body, auth).await?;

    if status >= 400 {
        return Ok(serde_json::json!({
            "error": format!("REPORT failed with status {status}"),
            "body": truncate(&text, 2000),
        }));
    }

    let tasks = parse_todos_from_multistatus(&text);

    Ok(serde_json::json!({
        "action": "list_tasks",
        "calendar_path": cal_path,
        "task_count": tasks.len(),
        "tasks": tasks,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn create_task(
    client: &Client,
    server_url: &str,
    cal_path: &str,
    summary: &str,
    description: &str,
    due: &str,
    priority: &str,
    auth: &Option<(String, String)>,
) -> ToolResult {
    let uid = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let mut vcal = format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//Marrow//CalDAV Tool//EN\r\n\
         BEGIN:VTODO\r\n\
         UID:{uid}\r\n\
         DTSTAMP:{now}\r\n\
         CREATED:{now}\r\n\
         SUMMARY:{summary}\r\n\
         STATUS:NEEDS-ACTION\r\n"
    );

    if !description.is_empty() {
        vcal.push_str(&format!("DESCRIPTION:{description}\r\n"));
    }
    if !due.is_empty() {
        let due_normalized = normalize_date(due);
        if due_normalized.contains('T') {
            vcal.push_str(&format!("DUE:{due_normalized}\r\n"));
        } else {
            vcal.push_str(&format!("DUE;VALUE=DATE:{due_normalized}\r\n"));
        }
    }
    if !priority.is_empty() {
        vcal.push_str(&format!("PRIORITY:{priority}\r\n"));
    }

    vcal.push_str("END:VTODO\r\nEND:VCALENDAR\r\n");

    let resource_url = format!(
        "{}/{uid}.ics",
        resolve_url(server_url, cal_path).trim_end_matches('/')
    );

    let mut builder = client
        .put(&resource_url)
        .header(CONTENT_TYPE, "text/calendar; charset=utf-8")
        .header("If-None-Match", "*")
        .body(vcal);

    if let Some((user, pass)) = auth {
        builder = builder.basic_auth(user, Some(pass));
    }

    let resp = builder.send().await?;
    let status = resp.status().as_u16();

    if status >= 400 {
        let body = resp.text().await.unwrap_or_default();
        return Ok(serde_json::json!({
            "error": format!("PUT failed with status {status}"),
            "body": truncate(&body, 2000),
        }));
    }

    Ok(serde_json::json!({
        "action": "create_task",
        "created_uid": uid,
        "summary": summary,
    }))
}

async fn complete_task(
    client: &Client,
    server_url: &str,
    cal_path: &str,
    uid: &str,
    auth: &Option<(String, String)>,
) -> ToolResult {
    // Fetch the existing task via REPORT
    let url = resolve_url(server_url, cal_path);
    let query_body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:D="DAV:">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VTODO">
        <C:prop-filter name="UID">
          <C:text-match>{uid}</C:text-match>
        </C:prop-filter>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#
    );

    let (status, text) = caldav_request(client, "REPORT", &url, &query_body, auth).await?;

    if status >= 400 {
        return Ok(serde_json::json!({
            "error": format!("failed to fetch task: status {status}")
        }));
    }

    let ical_data = extract_first_calendar_data(&text);
    if ical_data.is_empty() {
        return Ok(serde_json::json!({
            "error": format!("task with UID {uid} not found")
        }));
    }

    // Update STATUS to COMPLETED and add COMPLETED timestamp
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let updated = update_vtodo_status(&ical_data, "COMPLETED", &now);

    let resource_url = format!("{}/{uid}.ics", url.trim_end_matches('/'));

    let mut builder = client
        .put(&resource_url)
        .header(CONTENT_TYPE, "text/calendar; charset=utf-8")
        .body(updated);

    if let Some((user, pass)) = auth {
        builder = builder.basic_auth(user, Some(pass));
    }

    let resp = builder.send().await?;
    let put_status = resp.status().as_u16();

    if put_status >= 400 {
        let body = resp.text().await.unwrap_or_default();
        return Ok(serde_json::json!({
            "error": format!("PUT failed with status {put_status}"),
            "body": truncate(&body, 2000),
        }));
    }

    Ok(serde_json::json!({
        "action": "complete_task",
        "uid": uid,
        "status": "COMPLETED",
    }))
}

async fn delete_task(
    client: &Client,
    server_url: &str,
    cal_path: &str,
    uid: &str,
    auth: &Option<(String, String)>,
) -> ToolResult {
    let resource_url = format!(
        "{}/{uid}.ics",
        resolve_url(server_url, cal_path).trim_end_matches('/')
    );

    let mut builder = client.delete(&resource_url);

    if let Some((user, pass)) = auth {
        builder = builder.basic_auth(user, Some(pass));
    }

    let resp = builder.send().await?;
    let status = resp.status().as_u16();

    if status >= 400 {
        let body = resp.text().await.unwrap_or_default();
        return Ok(serde_json::json!({
            "error": format!("DELETE failed with status {status}"),
            "body": truncate(&body, 2000),
        }));
    }

    Ok(serde_json::json!({
        "action": "delete_task",
        "uid": uid,
        "deleted": true,
    }))
}

// ---------------------------------------------------------------------------
// iCalendar + XML parsing helpers
// ---------------------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max])
    } else {
        s.to_string()
    }
}

const DAV_RESPONSE: &str = "DAV::response";
const DAV_HREF: &str = "DAV::href";
const DAV_PROPSTAT: &str = "DAV::propstat";
const DAV_PROP: &str = "DAV::prop";
const DAV_DISPLAYNAME: &str = "DAV::displayname";
const DAV_RESOURCETYPE: &str = "DAV::resourcetype";
const DAV_STATUS: &str = "DAV::status";
const CALDAV_CALENDAR: &str = "urn:ietf:params:xml:ns:caldav:calendar";
const CALDAV_COMP_SET: &str = "urn:ietf:params:xml:ns:caldav:supported-calendar-component-set";
const CALDAV_COMP: &str = "urn:ietf:params:xml:ns:caldav:comp";
const CALDAV_DATA: &str = "urn:ietf:params:xml:ns:caldav:calendar-data";

use crate::xml::{self, XmlNode};

/// Find the first `DAV::propstat` with a 200 status, return its `DAV::prop`.
fn ok_prop(response: &XmlNode) -> Option<&XmlNode> {
    for propstat in response.find_all(DAV_PROPSTAT) {
        let is_ok = propstat
            .child_text(DAV_STATUS)
            .map(|s| s.contains("200"))
            .unwrap_or(true); // no status = assume OK
        if is_ok {
            return propstat.find(DAV_PROP);
        }
    }
    None
}

fn parse_calendar_list(body: &str) -> Vec<serde_json::Value> {
    let root = match xml::parse(body) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut calendars = Vec::new();
    for response in root.find_all(DAV_RESPONSE) {
        let href = response.child_text(DAV_HREF).unwrap_or_default();

        // Skip inbox/outbox/trashbin
        if href.ends_with("/inbox")
            || href.ends_with("/inbox/")
            || href.ends_with("/outbox")
            || href.ends_with("/outbox/")
            || href.ends_with("/trashbin")
            || href.ends_with("/trashbin/")
            || href.contains("contact_birthdays")
        {
            continue;
        }

        let prop = match ok_prop(response) {
            Some(p) => p,
            None => continue,
        };

        // Must be a calendar collection
        let rtype = match prop.find(DAV_RESOURCETYPE) {
            Some(r) => r,
            None => continue,
        };
        if !rtype.has_child(CALDAV_CALENDAR) {
            continue;
        }

        let displayname = prop.child_text(DAV_DISPLAYNAME).unwrap_or_default();

        let mut supports_events = false;
        let mut supports_tasks = false;
        if let Some(comp_set) = prop.find(CALDAV_COMP_SET) {
            for comp in comp_set.find_all(CALDAV_COMP) {
                match comp.attrs.get("name").map(|s| s.as_str()) {
                    Some("VEVENT") => supports_events = true,
                    Some("VTODO") => supports_tasks = true,
                    _ => {}
                }
            }
        }

        calendars.push(serde_json::json!({
            "href": href,
            "displayname": displayname,
            "supports_events": supports_events,
            "supports_tasks": supports_tasks,
        }));
    }

    calendars
}

fn extract_all_calendar_data(body: &str) -> Vec<String> {
    let root = match xml::parse(body) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    for response in root.find_all(DAV_RESPONSE) {
        if let Some(prop) = ok_prop(response)
            && let Some(data_node) = prop.find(CALDAV_DATA)
            && let Some(text) = &data_node.text
            && !text.is_empty()
        {
            results.push(text.clone());
        }
    }
    results
}

fn extract_first_calendar_data(body: &str) -> String {
    extract_all_calendar_data(body)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn parse_events_from_multistatus(xml: &str) -> Vec<serde_json::Value> {
    let mut events = Vec::new();

    for ical_data in extract_all_calendar_data(xml) {
        for component in parse_ical_components(&ical_data, "VEVENT") {
            events.push(serde_json::json!({
                "uid": component.get("UID").unwrap_or(&String::new()),
                "summary": component.get("SUMMARY").unwrap_or(&String::new()),
                "dtstart": component.get("DTSTART").unwrap_or(&String::new()),
                "dtend": component.get("DTEND").unwrap_or(&String::new()),
                "location": component.get("LOCATION").unwrap_or(&String::new()),
                "description": component.get("DESCRIPTION").unwrap_or(&String::new()),
                "status": component.get("STATUS").unwrap_or(&String::new()),
            }));
        }
    }

    events
}

fn parse_todos_from_multistatus(xml: &str) -> Vec<serde_json::Value> {
    let mut tasks = Vec::new();

    for ical_data in extract_all_calendar_data(xml) {
        for component in parse_ical_components(&ical_data, "VTODO") {
            tasks.push(serde_json::json!({
                "uid": component.get("UID").unwrap_or(&String::new()),
                "summary": component.get("SUMMARY").unwrap_or(&String::new()),
                "status": component.get("STATUS").unwrap_or(&String::new()),
                "due": component.get("DUE").unwrap_or(&String::new()),
                "priority": component.get("PRIORITY").unwrap_or(&String::new()),
                "description": component.get("DESCRIPTION").unwrap_or(&String::new()),
                "completed": component.get("COMPLETED").unwrap_or(&String::new()),
                "percent_complete": component.get("PERCENT-COMPLETE").unwrap_or(&String::new()),
                "created": component.get("CREATED").unwrap_or(&String::new()),
            }));
        }
    }

    tasks
}

fn parse_ical_components(ical: &str, component_type: &str) -> Vec<HashMap<String, String>> {
    let mut components = Vec::new();
    let begin_marker = format!("BEGIN:{component_type}");
    let end_marker = format!("END:{component_type}");

    // Unfold iCalendar line continuations (RFC 5545 §3.1)
    let unfolded = ical.replace("\r\n ", "").replace("\r\n\t", "");

    let mut search = unfolded.as_str();
    while let Some(begin) = search.find(&begin_marker) {
        let after_begin = &search[begin + begin_marker.len()..];
        if let Some(end) = after_begin.find(&end_marker) {
            let block = &after_begin[..end];
            let mut props = HashMap::new();

            for line in block.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with("BEGIN:") || line.starts_with("END:") {
                    continue;
                }
                // Property format: NAME;params:value or NAME:value
                if let Some(colon_pos) = line.find(':') {
                    let key_part = &line[..colon_pos];
                    let value = &line[colon_pos + 1..];
                    // Strip parameters (e.g. DTSTART;TZID=...:20240101)
                    let key = key_part.split(';').next().unwrap_or(key_part);
                    props.insert(key.to_uppercase(), value.to_string());
                }
            }

            components.push(props);
            search = &after_begin[end + end_marker.len()..];
        } else {
            break;
        }
    }

    components
}

fn update_vtodo_status(ical: &str, new_status: &str, completed_stamp: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_vtodo = false;
    let mut status_set = false;
    let mut completed_set = false;

    // Unfold then process
    let unfolded = ical.replace("\r\n ", "").replace("\r\n\t", "");

    for line in unfolded.lines() {
        if line.starts_with("BEGIN:VTODO") {
            in_vtodo = true;
            lines.push(line.to_string());
            continue;
        }
        if line.starts_with("END:VTODO") {
            if !status_set {
                lines.push(format!("STATUS:{new_status}"));
            }
            if !completed_set && new_status == "COMPLETED" {
                lines.push(format!("COMPLETED:{completed_stamp}"));
                lines.push("PERCENT-COMPLETE:100".to_string());
            }
            in_vtodo = false;
            lines.push(line.to_string());
            continue;
        }
        if in_vtodo {
            let key = line
                .split(';')
                .next()
                .unwrap_or(line)
                .split(':')
                .next()
                .unwrap_or("");
            if key == "STATUS" {
                lines.push(format!("STATUS:{new_status}"));
                status_set = true;
                continue;
            }
            if key == "COMPLETED" {
                lines.push(format!("COMPLETED:{completed_stamp}"));
                completed_set = true;
                continue;
            }
        }
        lines.push(line.to_string());
    }

    lines.join("\r\n") + "\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_date_iso_to_ical() {
        assert_eq!(normalize_date("2026-04-20T10:00:00Z"), "20260420T100000Z");
        assert_eq!(normalize_date("2026-04-20"), "20260420");
        assert_eq!(normalize_date("2026-04-20T10:00"), "20260420T1000");
    }

    #[test]
    fn normalize_date_passthrough() {
        assert_eq!(normalize_date("20260420T100000Z"), "20260420T100000Z");
        assert_eq!(normalize_date("20260420"), "20260420");
        assert_eq!(normalize_date(""), "");
    }

    #[test]
    fn parse_ical_vevent() {
        let ical = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc123\r\nSUMMARY:Test Event\r\nDTSTART:20260420T100000Z\r\nDTEND:20260420T110000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let components = parse_ical_components(ical, "VEVENT");
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].get("UID").unwrap(), "abc123");
        assert_eq!(components[0].get("SUMMARY").unwrap(), "Test Event");
        assert_eq!(components[0].get("DTSTART").unwrap(), "20260420T100000Z");
    }

    #[test]
    fn parse_ical_vtodo() {
        let ical = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:task1\r\nSUMMARY:Buy milk\r\nSTATUS:NEEDS-ACTION\r\nDUE:20260421\r\nPRIORITY:5\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let components = parse_ical_components(ical, "VTODO");
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].get("UID").unwrap(), "task1");
        assert_eq!(components[0].get("SUMMARY").unwrap(), "Buy milk");
        assert_eq!(components[0].get("STATUS").unwrap(), "NEEDS-ACTION");
        assert_eq!(components[0].get("DUE").unwrap(), "20260421");
    }

    #[test]
    fn parse_ical_with_params() {
        let ical = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x1\r\nDTSTART;TZID=Europe/Stockholm:20260420T100000\r\nSUMMARY:Meeting\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let components = parse_ical_components(ical, "VEVENT");
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].get("DTSTART").unwrap(), "20260420T100000");
    }

    #[test]
    fn update_vtodo_completes() {
        let ical = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:t1\r\nSTATUS:NEEDS-ACTION\r\nSUMMARY:Do thing\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let updated = update_vtodo_status(ical, "COMPLETED", "20260420T120000Z");
        assert!(updated.contains("STATUS:COMPLETED"));
        assert!(updated.contains("COMPLETED:20260420T120000Z"));
        assert!(updated.contains("PERCENT-COMPLETE:100"));
        assert!(!updated.contains("NEEDS-ACTION"));
    }

    #[test]
    fn resolve_url_combinations() {
        assert_eq!(
            resolve_url("https://cloud.example.com", "/dav/calendars/user/cal/"),
            "https://cloud.example.com/dav/calendars/user/cal/"
        );
        assert_eq!(
            resolve_url("https://cloud.example.com/", "/dav/cal/"),
            "https://cloud.example.com/dav/cal/"
        );
        assert_eq!(
            resolve_url("https://x.com", "https://other.com/path"),
            "https://other.com/path"
        );
    }
}
