use std::collections::HashMap;

use uuid::Uuid;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct MemoryUpdateTool;

impl Tool for MemoryUpdateTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "memory_update".to_string(),
            description: "Updates an existing stored fact by its ID".to_string(),
            provides: vec!["memory_update".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![ParamDef::required("ID"), ParamDef::required("FACT")]
    }

    fn returns(&self) -> Vec<String> {
        vec!["id".to_string(), "fact".to_string(), "status".to_string()]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let store = match &ctx.memory_store {
                Some(s) => s,
                None => {
                    return Ok(serde_json::json!({"error": "memory store not available"}));
                }
            };

            let id_str = match params.get("ID") {
                Some(s) if !s.is_empty() => s,
                _ => {
                    return Ok(serde_json::json!({"error": "missing required parameter: ID"}));
                }
            };

            let id: Uuid = match id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    return Ok(serde_json::json!({"error": format!("invalid UUID: {id_str}")}));
                }
            };

            let new_fact = match params.get("FACT") {
                Some(f) if !f.is_empty() => f.clone(),
                _ => {
                    return Ok(serde_json::json!({"error": "missing required parameter: FACT"}));
                }
            };

            match store.update(id, new_fact.clone()) {
                Ok(()) => Ok(serde_json::json!({
                    "id": id.to_string(),
                    "fact": new_fact,
                    "status": "updated",
                })),
                Err(e) => Ok(serde_json::json!({"error": format!("failed to update: {e}")})),
            }
        })
    }
}
