use std::collections::HashMap;

use serde_json::{Value, json};

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

const LOOKUP_URL: &str = "https://services.c.web.sl.se/locationwebservice/lookup";
const PLANNER_URL: &str = "https://services.c.web.sl.se/journeywebservice-sl/planner";

pub struct SlTransitTool;

impl Tool for SlTransitTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "sl_transit".to_string(),
            description: "Search SL (Stockholm public transit) stops and plan journeys. \
                 Actions: lookup (search stops by name, returns placeId), \
                 journey (plan route using ORIGIN_PLACE_ID + DESTINATION_PLACE_ID from lookup). \
                 Always lookup first to get placeIds, then plan journey"
                .to_string(),
            provides: vec!["sl_transit".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::required("ACTION"),
            ParamDef::optional("QUERY"),
            ParamDef::optional("ORIGIN_PLACE_ID"),
            ParamDef::optional("DESTINATION_PLACE_ID"),
            ParamDef::optional("WHEN"),
            ParamDef::optional("ARRIVAL"),
            ParamDef::optional("TRANSPORT_TYPE"),
        ]
    }

    fn returns(&self) -> Vec<String> {
        vec!["action".to_string(), "results".to_string()]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let action = match params.get("ACTION") {
                Some(a) if !a.is_empty() => a.as_str(),
                _ => {
                    return Ok(
                        json!({"error": "missing required parameter: ACTION (lookup, journey)"}),
                    );
                }
            };

            match action {
                "lookup" => lookup(&params, &ctx).await,
                "journey" => journey(&params, &ctx).await,
                other => {
                    Ok(json!({"error": format!("unknown action: {other}. Use: lookup, journey")}))
                }
            }
        })
    }
}

async fn lookup(
    params: &HashMap<String, String>,
    ctx: &ToolContext,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let query = match params.get("QUERY") {
        Some(q) if !q.is_empty() => q,
        _ => return Ok(json!({"error": "missing required parameter: QUERY"})),
    };

    let url = reqwest::Url::parse_with_params(LOOKUP_URL, &[("search", query.as_str())])
        .map_err(|e| format!("invalid lookup URL: {e}"))?;

    let resp = match ctx.client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return Ok(json!({"error": format!("lookup request failed: {e}")})),
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Ok(json!({"error": format!("lookup returned {status}"), "body": body}));
    }

    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Ok(json!({"error": format!("failed to decode lookup response: {e}")})),
    };

    let mut locations = Vec::new();
    collect_lookup_items(&body, &mut locations);

    Ok(json!({
        "action": "lookup",
        "results": locations,
    }))
}

fn collect_lookup_items(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_lookup_items(item, out);
            }
        }
        Value::Object(obj) => {
            if obj.contains_key("placeId")
                && obj.contains_key("name")
                && obj.contains_key("locationType")
            {
                out.push(json!({
                    "placeId": obj.get("placeId").cloned().unwrap_or(Value::Null),
                    "name": obj.get("name").cloned().unwrap_or(Value::Null),
                    "locationType": obj.get("locationType").cloned().unwrap_or(Value::Null),
                }));
            }
            for child in obj.values() {
                collect_lookup_items(child, out);
            }
        }
        _ => {}
    }
}

async fn journey(
    params: &HashMap<String, String>,
    ctx: &ToolContext,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let origin = match params.get("ORIGIN_PLACE_ID") {
        Some(o) if !o.is_empty() => o,
        _ => return Ok(json!({"error": "missing required parameter: ORIGIN_PLACE_ID"})),
    };
    let destination = match params.get("DESTINATION_PLACE_ID") {
        Some(d) if !d.is_empty() => d,
        _ => return Ok(json!({"error": "missing required parameter: DESTINATION_PLACE_ID"})),
    };

    let search_for_arrival = params
        .get("ARRIVAL")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));

    let transport_type = params
        .get("TRANSPORT_TYPE")
        .filter(|v| !v.is_empty())
        .map(|v| v.to_uppercase());

    let mut payload = serde_json::Map::new();
    payload.insert("origin".to_string(), json!({"placeId": origin}));
    payload.insert("destination".to_string(), json!({"placeId": destination}));
    payload.insert(
        "searchForArrival".to_string(),
        Value::Bool(search_for_arrival),
    );
    payload.insert("includeExternalOperators".to_string(), Value::Bool(true));

    if let Some(when) = params.get("WHEN").filter(|v| !v.is_empty()) {
        match normalize_datetime(when) {
            Ok(dt) => {
                payload.insert("dateTime".to_string(), Value::String(dt));
            }
            Err(e) => return Ok(json!({"error": e})),
        }
    }

    let resp = match ctx
        .client
        .post(PLANNER_URL)
        .header("Content-Type", "application/json")
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36")
        .header("Accept-Language", "sv-SE,sv;q=0.9,en;q=0.8")
        .json(&Value::Object(payload))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(json!({"error": format!("journey request failed: {e}")})),
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Ok(json!({"error": format!("journey returned {status}"), "body": body}));
    }

    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Ok(json!({"error": format!("failed to decode journey response: {e}")})),
    };

    let mut journeys = match body {
        Value::Array(items) => items,
        other => vec![other],
    };

    if let Some(tt) = &transport_type {
        journeys.retain(|j| value_contains_transport_type(j, tt));
    }

    Ok(json!({
        "action": "journey",
        "results": journeys,
    }))
}

fn normalize_datetime(input: &str) -> Result<String, String> {
    let normalized = input.replace(' ', "T");
    let bytes = normalized.as_bytes();
    let valid = bytes.len() == 16
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, b)| matches!(idx, 4 | 7 | 10 | 13) || b.is_ascii_digit());

    if !valid {
        return Err("invalid WHEN value: expected YYYY-MM-DDTHH:MM or YYYY-MM-DD HH:MM".into());
    }

    Ok(normalized)
}

fn value_contains_transport_type(value: &Value, wanted_upper: &str) -> bool {
    match value {
        Value::String(s) => s.to_uppercase() == wanted_upper,
        Value::Array(items) => items
            .iter()
            .any(|item| value_contains_transport_type(item, wanted_upper)),
        Value::Object(obj) => obj.iter().any(|(k, v)| {
            let lower = k.to_ascii_lowercase();
            let is_type_key = lower.contains("transport")
                || lower.contains("product")
                || lower.contains("mode")
                || lower == "type";
            if is_type_key {
                value_contains_transport_type(v, wanted_upper)
            } else {
                // Recurse into child objects/arrays
                matches!(v, Value::Object(_) | Value::Array(_))
                    && value_contains_transport_type(v, wanted_upper)
            }
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_datetime_iso() {
        assert_eq!(
            normalize_datetime("2026-04-21T08:30").unwrap(),
            "2026-04-21T08:30"
        );
    }

    #[test]
    fn normalize_datetime_space() {
        assert_eq!(
            normalize_datetime("2026-04-21 08:30").unwrap(),
            "2026-04-21T08:30"
        );
    }

    #[test]
    fn normalize_datetime_rejects_bad_format() {
        assert!(normalize_datetime("2026-04-21").is_err());
        assert!(normalize_datetime("not-a-date").is_err());
        assert!(normalize_datetime("").is_err());
    }

    #[test]
    fn collect_lookup_extracts_place_objects() {
        let data = json!({
            "results": [
                {
                    "placeId": "abc123",
                    "name": "T-Centralen",
                    "locationType": "STATION",
                    "extra": "ignored"
                },
                {
                    "placeId": "def456",
                    "name": "Slussen",
                    "locationType": "STATION"
                }
            ]
        });

        let mut out = Vec::new();
        collect_lookup_items(&data, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["name"], "T-Centralen");
        assert_eq!(out[1]["placeId"], "def456");
        // extra field should not be present
        assert!(out[0].get("extra").is_none());
    }

    #[test]
    fn collect_lookup_handles_empty() {
        let mut out = Vec::new();
        collect_lookup_items(&json!({}), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn transport_type_filter_matches() {
        let journey = json!({
            "legs": [{
                "transport": "BUS",
                "line": "4"
            }]
        });
        assert!(value_contains_transport_type(&journey, "BUS"));
        assert!(!value_contains_transport_type(&journey, "TRAIN"));
    }

    #[test]
    fn transport_type_case_insensitive() {
        let journey = json!({"transportMode": "bus"});
        assert!(value_contains_transport_type(&journey, "BUS"));
    }

    #[tokio::test]
    #[ignore] // hits real SL API
    async fn live_lookup_and_journey() {
        use crate::secrets::Secrets;
        use std::sync::Arc;

        let ctx = ToolContext {
            client: Arc::new(reqwest::Client::new()),
            secrets: Arc::new(Secrets::default()),
            task_description: "test".to_string(),
            schedule_store: None,
            memory_store: None,
            frontend_context: None,
        };
        let tool = SlTransitTool;

        // Lookup T-Centralen
        let mut params = HashMap::new();
        params.insert("ACTION".into(), "lookup".into());
        params.insert("QUERY".into(), "T-Centralen".into());
        let result = tool.execute(params, ctx.clone()).await.unwrap();
        assert!(result.get("error").is_none(), "lookup error: {result}");

        let locations = result["results"].as_array().unwrap();
        assert!(!locations.is_empty(), "expected at least one location");
        let origin_id = locations[0]["placeId"].as_str().unwrap();
        println!(
            "Lookup found {} locations, first: {}",
            locations.len(),
            locations[0]["name"]
        );

        // Lookup Slussen
        let mut params2 = HashMap::new();
        params2.insert("ACTION".into(), "lookup".into());
        params2.insert("QUERY".into(), "Slussen".into());
        let result2 = tool.execute(params2, ctx.clone()).await.unwrap();
        assert!(result2.get("error").is_none(), "lookup error: {result2}");

        let locations2 = result2["results"].as_array().unwrap();
        let dest_id = locations2[0]["placeId"].as_str().unwrap();
        println!("Destination: {} ({})", locations2[0]["name"], dest_id);

        // Journey from T-Centralen to Slussen
        let mut params3 = HashMap::new();
        params3.insert("ACTION".into(), "journey".into());
        params3.insert("ORIGIN_PLACE_ID".into(), origin_id.into());
        params3.insert("DESTINATION_PLACE_ID".into(), dest_id.into());
        let result3 = tool.execute(params3, ctx).await.unwrap();
        assert!(result3.get("error").is_none(), "journey error: {result3}");

        let journeys = result3["results"].as_array().unwrap();
        assert!(!journeys.is_empty(), "expected at least one journey");
        println!("Got {} journey option(s)", journeys.len());
    }
}
