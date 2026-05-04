use std::collections::HashMap;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct StateSetTool;

impl Tool for StateSetTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "state_set".to_string(),
            description: "Sets a state value for a key, or deletes the key if VALUE is omitted"
                .to_string(),
            provides: vec!["state_set".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![ParamDef::required("KEY"), ParamDef::optional("VALUE")]
    }

    fn returns(&self) -> Vec<String> {
        vec!["key".to_string(), "status".to_string()]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let store = match &ctx.memory_store {
                Some(s) => s,
                None => {
                    return Ok(serde_json::json!({"error": "memory store not available"}));
                }
            };

            let key = match params.get("KEY") {
                Some(k) if !k.is_empty() => k,
                _ => {
                    return Ok(serde_json::json!({"error": "missing required parameter: KEY"}));
                }
            };

            match params.get("VALUE") {
                Some(value) if !value.is_empty() => match store.kv_set(key, value) {
                    Ok(()) => Ok(serde_json::json!({
                        "key": key,
                        "value": value,
                        "status": "saved",
                    })),
                    Err(e) => Ok(serde_json::json!({
                        "error": format!("failed to set state: {e}"),
                    })),
                },
                _ => match store.kv_delete(key) {
                    Ok(true) => Ok(serde_json::json!({
                        "key": key,
                        "status": "deleted",
                    })),
                    Ok(false) => Ok(serde_json::json!({
                        "key": key,
                        "status": "not_found",
                    })),
                    Err(e) => Ok(serde_json::json!({
                        "error": format!("failed to delete state: {e}"),
                    })),
                },
            }
        })
    }
}
