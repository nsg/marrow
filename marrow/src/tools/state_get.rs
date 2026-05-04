use std::collections::HashMap;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct StateGetTool;

impl Tool for StateGetTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "state_get".to_string(),
            description: "Gets a stored state value by key, or lists all keys if KEY is omitted"
                .to_string(),
            provides: vec!["state_get".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![ParamDef::optional("KEY")]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "key".to_string(),
            "value".to_string(),
            "updated".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let store = match &ctx.memory_store {
                Some(s) => s,
                None => {
                    return Ok(serde_json::json!({"error": "memory store not available"}));
                }
            };

            match params.get("KEY") {
                Some(key) if !key.is_empty() => match store.kv_get(key) {
                    Ok(Some((value, updated))) => Ok(serde_json::json!({
                        "key": key,
                        "value": value,
                        "updated": updated,
                    })),
                    Ok(None) => Ok(serde_json::json!({
                        "key": key,
                        "value": null,
                    })),
                    Err(e) => Ok(serde_json::json!({
                        "error": format!("failed to get state: {e}"),
                    })),
                },
                _ => match store.kv_list() {
                    Ok(entries) => {
                        let items: Vec<serde_json::Value> = entries
                            .iter()
                            .map(|e| {
                                serde_json::json!({
                                    "key": e.key,
                                    "value": e.value,
                                    "updated": e.updated,
                                })
                            })
                            .collect();
                        Ok(serde_json::json!({
                            "entries": items,
                            "count": items.len(),
                        }))
                    }
                    Err(e) => Ok(serde_json::json!({
                        "error": format!("failed to list state: {e}"),
                    })),
                },
            }
        })
    }
}
