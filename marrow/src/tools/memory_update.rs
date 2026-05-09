use std::collections::HashMap;

use uuid::Uuid;

use crate::memory::{Memory, MemorySource};
use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct MemoryUpdateTool;

impl Tool for MemoryUpdateTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "memory_update".to_string(),
            description: "Creates or updates a stored fact. Omit ID to create a new memory (server assigns the ID). Provide ID to update an existing one.".to_string(),
            provides: vec!["memory_update".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![
            ParamDef::optional("ID"),
            ParamDef::required("FACT"),
            ParamDef::optional("SOURCE"),
        ]
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

            let fact = match params.get("FACT") {
                Some(f) if !f.is_empty() => f.clone(),
                _ => {
                    return Ok(serde_json::json!({"error": "missing required parameter: FACT"}));
                }
            };

            let source = match params.get("SOURCE") {
                Some(s) if !s.is_empty() => match MemorySource::from_db_str(s) {
                    Ok(src) => Some(src),
                    Err(_) => {
                        return Ok(
                            serde_json::json!({"error": format!("invalid SOURCE: {s} (expected 'user' or 'auto')")}),
                        );
                    }
                },
                _ => None,
            };

            match params.get("ID").filter(|s| !s.is_empty()) {
                Some(id_str) => {
                    let id: Uuid = match id_str.parse() {
                        Ok(id) => id,
                        Err(_) => {
                            return Ok(
                                serde_json::json!({"error": format!("invalid UUID: {id_str}")}),
                            );
                        }
                    };
                    match store.update_with_source(id, fact.clone(), source) {
                        Ok(()) => Ok(serde_json::json!({
                            "id": id.to_string(),
                            "fact": fact,
                            "status": "updated",
                        })),
                        Err(e) => {
                            Ok(serde_json::json!({"error": format!("failed to update: {e}")}))
                        }
                    }
                }
                None => {
                    let memory = Memory::new(fact, source.unwrap_or(MemorySource::User));
                    let id = memory.id;
                    match store.save(&memory) {
                        Ok(()) => Ok(serde_json::json!({
                            "id": id.to_string(),
                            "fact": memory.fact,
                            "status": "created",
                        })),
                        Err(e) => {
                            Ok(serde_json::json!({"error": format!("failed to create: {e}")}))
                        }
                    }
                }
            }
        })
    }
}
