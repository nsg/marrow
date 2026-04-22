use std::collections::HashMap;

use uuid::Uuid;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct MemoryDeleteTool;

impl Tool for MemoryDeleteTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "memory_delete".to_string(),
            description: "Deletes a stored fact by its ID".to_string(),
            provides: vec!["memory_delete".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![ParamDef::required("ID")]
    }

    fn returns(&self) -> Vec<String> {
        vec!["id".to_string(), "status".to_string()]
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

            match store.delete(id) {
                Ok(()) => Ok(serde_json::json!({
                    "id": id.to_string(),
                    "status": "deleted",
                })),
                Err(e) => Ok(serde_json::json!({"error": format!("failed to delete: {e}")})),
            }
        })
    }
}
