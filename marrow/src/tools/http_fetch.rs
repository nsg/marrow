use std::collections::HashMap;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct HttpFetchTool;

fn truncate_chars_with_total(s: String, limit: usize) -> String {
    let mut chars = s.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!(
            "{}...[truncated, {} chars total]",
            truncated,
            s.chars().count()
        )
    } else {
        s
    }
}

impl Tool for HttpFetchTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "http_fetch".to_string(),
            description: "Makes an HTTP request and returns the response status, headers, and body"
                .to_string(),
            provides: vec!["http_fetch".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::required("URL"),
            ParamDef::optional("METHOD"),
            ParamDef::optional("HEADERS"),
            ParamDef::optional("BODY"),
            ParamDef::optional("TIMEOUT_SECS"),
        ]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "status".to_string(),
            "headers".to_string(),
            "body".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let url = match params.get("URL") {
                Some(u) if !u.is_empty() => u.clone(),
                _ => {
                    return Ok(serde_json::json!({"error": "missing required parameter: URL"}));
                }
            };

            let method = params
                .get("METHOD")
                .map(|m| m.to_uppercase())
                .unwrap_or_else(|| "GET".to_string());

            let timeout_secs: u64 = params
                .get("TIMEOUT_SECS")
                .and_then(|t| t.parse().ok())
                .unwrap_or(30);

            let mut builder = match method.as_str() {
                "GET" => ctx.client.get(&url),
                "POST" => ctx.client.post(&url),
                "PUT" => ctx.client.put(&url),
                "PATCH" => ctx.client.patch(&url),
                "DELETE" => ctx.client.delete(&url),
                "HEAD" => ctx.client.head(&url),
                other => {
                    return Ok(
                        serde_json::json!({"error": format!("unsupported method: {other}")}),
                    );
                }
            };

            builder = builder.timeout(std::time::Duration::from_secs(timeout_secs));

            if let Some(headers_json) = params.get("HEADERS")
                && !headers_json.is_empty()
            {
                match serde_json::from_str::<HashMap<String, String>>(headers_json) {
                    Ok(headers) => {
                        for (key, value) in &headers {
                            builder = builder.header(key.as_str(), value.as_str());
                        }
                    }
                    Err(e) => {
                        return Ok(serde_json::json!({
                            "error": format!("invalid HEADERS JSON: {e}")
                        }));
                    }
                }
            }

            if let Some(body) = params.get("BODY")
                && !body.is_empty()
            {
                builder = builder.body(body.clone());
            }

            let resp = match builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    return Ok(serde_json::json!({
                        "error": format!("request failed: {e}")
                    }));
                }
            };

            let status = resp.status().as_u16();
            let resp_headers: HashMap<String, String> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            let body = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    return Ok(serde_json::json!({
                        "error": format!("failed to read response body: {e}"),
                        "status": status,
                        "headers": resp_headers,
                    }));
                }
            };

            // Truncate very large responses to avoid blowing up context
            let body_truncated = truncate_chars_with_total(body, 50_000);

            Ok(serde_json::json!({
                "status": status,
                "headers": resp_headers,
                "body": body_truncated,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_with_total_handles_non_ascii() {
        let truncated = truncate_chars_with_total("åäö🙂abcd".to_string(), 4);
        assert_eq!(truncated, "åäö🙂...[truncated, 8 chars total]");
    }
}
