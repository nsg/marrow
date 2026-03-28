use mlua::{Lua, LuaSerdeExt, Result, Value};
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::sandbox::create_sandbox;

const MAX_RECURSION_DEPTH: u32 = 5;

pub struct HostConfig {
    pub client: Arc<Client>,
    pub toolbox_dir: Option<PathBuf>,
    pub task_description: String,
    pub recursion_depth: Arc<AtomicU32>,
}

impl HostConfig {
    pub fn simple(client: Arc<Client>) -> Self {
        Self {
            client,
            toolbox_dir: None,
            task_description: String::new(),
            recursion_depth: Arc::new(AtomicU32::new(0)),
        }
    }
}

pub fn register_host_functions(lua: &Lua, config: &HostConfig) -> Result<()> {
    register_http_get(lua, config.client.clone())?;
    register_http_post(lua, config.client.clone())?;
    register_json_parse(lua)?;
    register_json_encode(lua)?;
    register_log(lua)?;

    if let Some(ref toolbox_dir) = config.toolbox_dir {
        register_run_tool(
            lua,
            config.client.clone(),
            toolbox_dir.clone(),
            config.task_description.clone(),
            config.recursion_depth.clone(),
        )?;
    }

    Ok(())
}

fn register_run_tool(
    lua: &Lua,
    client: Arc<Client>,
    toolbox_dir: PathBuf,
    task_description: String,
    depth: Arc<AtomicU32>,
) -> Result<()> {
    let func =
        lua.create_async_function(move |lua, (name, params): (String, Option<mlua::Table>)| {
            let client = client.clone();
            let toolbox_dir = toolbox_dir.clone();
            let task_description = task_description.clone();
            let depth = depth.clone();
            async move {
                let current = depth.fetch_add(1, Ordering::SeqCst);
                if current >= MAX_RECURSION_DEPTH {
                    depth.fetch_sub(1, Ordering::SeqCst);
                    return Err(mlua::Error::external(format!(
                        "run_tool('{name}'): max recursion depth ({MAX_RECURSION_DEPTH}) exceeded"
                    )));
                }

                let result = run_tool_inner(
                    &name,
                    params.as_ref(),
                    &client,
                    &toolbox_dir,
                    &task_description,
                    &depth,
                )
                .await;

                depth.fetch_sub(1, Ordering::SeqCst);

                let json_value = result.map_err(|e| {
                    mlua::Error::external(format!("run_tool('{name}'): {e}"))
                })?;

                lua.to_value(&json_value)
            }
        })?;
    lua.globals().set("run_tool", func)?;
    Ok(())
}

async fn run_tool_inner(
    name: &str,
    params: Option<&mlua::Table>,
    client: &Client,
    toolbox_dir: &Path,
    task_description: &str,
    depth: &Arc<AtomicU32>,
) -> std::result::Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let lua_path = toolbox_dir.join(format!("{name}.lua"));
    let source = std::fs::read_to_string(&lua_path)
        .map_err(|e| format!("failed to load tool '{name}': {e}"))?;

    let inner_lua = create_sandbox()?;

    let inner_config = HostConfig {
        client: Arc::new(client.clone()),
        toolbox_dir: Some(toolbox_dir.to_path_buf()),
        task_description: task_description.to_string(),
        recursion_depth: depth.clone(),
    };
    register_host_functions(&inner_lua, &inner_config)?;

    // Set TASK table
    let task_table = inner_lua.create_table()?;
    task_table.set("description", task_description.to_string())?;
    inner_lua.globals().set("TASK", task_table)?;

    // Set PARAMS table from caller's params
    let params_table = inner_lua.create_table()?;
    if let Some(p) = params {
        for pair in p.pairs::<String, mlua::Value>() {
            let (k, v) = pair?;
            params_table.set(k, inner_lua.to_value(&inner_lua.from_value::<serde_json::Value>(v)?)?)?;
        }
    }
    inner_lua.globals().set("PARAMS", params_table)?;

    let result: mlua::Value = inner_lua.load(&source).eval_async().await?;
    let json: serde_json::Value = inner_lua.from_value(result)?;

    Ok(json)
}

fn register_http_get(lua: &Lua, client: Arc<Client>) -> Result<()> {
    let func = lua.create_async_function(move |lua, url: String| {
        let client = client.clone();
        async move {
            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(mlua::Error::external)?;

            let status = resp.status().as_u16();
            let body = resp.text().await.map_err(mlua::Error::external)?;

            let result = lua.create_table()?;
            result.set("status", status)?;
            result.set("body", body)?;
            Ok(Value::Table(result))
        }
    })?;
    lua.globals().set("http_get", func)?;
    Ok(())
}

fn register_http_post(lua: &Lua, client: Arc<Client>) -> Result<()> {
    let func = lua.create_async_function(move |lua, (url, body): (String, String)| {
        let client = client.clone();
        async move {
            let resp = client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(mlua::Error::external)?;

            let status = resp.status().as_u16();
            let resp_body = resp.text().await.map_err(mlua::Error::external)?;

            let result = lua.create_table()?;
            result.set("status", status)?;
            result.set("body", resp_body)?;
            Ok(Value::Table(result))
        }
    })?;
    lua.globals().set("http_post", func)?;
    Ok(())
}

fn register_json_parse(lua: &Lua) -> Result<()> {
    let func = lua.create_function(|lua, input: String| {
        let value: serde_json::Value =
            serde_json::from_str(&input).map_err(mlua::Error::external)?;
        lua.to_value(&value)
    })?;
    lua.globals().set("json_parse", func)?;
    Ok(())
}

fn register_json_encode(lua: &Lua) -> Result<()> {
    let func = lua.create_function(|lua, value: Value| {
        let json: serde_json::Value = lua.from_value(value)?;
        let output = serde_json::to_string(&json).map_err(mlua::Error::external)?;
        Ok(output)
    })?;
    lua.globals().set("json_encode", func)?;
    Ok(())
}

fn register_log(lua: &Lua) -> Result<()> {
    let func = lua.create_function(|_, msg: String| {
        eprintln!("[lua] {msg}");
        Ok(())
    })?;
    lua.globals().set("log", func)?;
    Ok(())
}
