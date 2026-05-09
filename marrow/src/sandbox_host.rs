use mlua::{Lua, LuaSerdeExt, Result, Value};
use reqwest::Client;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::memory::MemoryStore;
use crate::sandbox::create_sandbox;
use crate::schedule::ScheduleStore;
use crate::secrets::Secrets;
use crate::tool::{FrontendContext, Tool, ToolContext};
use crate::xml::XmlNode;

const MAX_RECURSION_DEPTH: u32 = 5;

const TOOLBOX_PREFIX: &str = "tool_";

pub struct HostConfig {
    pub client: Arc<Client>,
    pub toolbox_dir: Option<PathBuf>,
    pub task_description: String,
    pub recursion_depth: Arc<AtomicU32>,
    pub secrets: Arc<HashMap<String, String>>,
    pub builtins: Arc<HashMap<String, Arc<dyn Tool>>>,
    pub schedule_store: Option<Arc<ScheduleStore>>,
    pub memory_store: Option<Arc<MemoryStore>>,
    pub frontend_context: Option<FrontendContext>,
}

impl HostConfig {
    pub fn simple(client: Arc<Client>) -> Self {
        Self {
            client,
            toolbox_dir: None,
            task_description: String::new(),
            recursion_depth: Arc::new(AtomicU32::new(0)),
            secrets: Arc::new(HashMap::new()),
            builtins: Arc::new(HashMap::new()),
            schedule_store: None,
            memory_store: None,
            frontend_context: None,
        }
    }
}

pub fn register_host_functions(lua: &Lua, config: &HostConfig) -> Result<()> {
    register_http_request(lua, config.client.clone())?;
    register_http_get(lua, config.client.clone())?;
    register_http_post(lua, config.client.clone())?;
    register_json_parse(lua)?;
    register_json_encode(lua)?;
    register_xml_parse(lua)?;
    register_xml_encode(lua)?;
    register_log(lua)?;
    register_secret(lua, config.secrets.clone())?;

    register_run_tool(lua, config)?;
    register_builtin_tools(lua, config)?;
    register_toolbox_tools(lua, config)?;

    Ok(())
}

fn register_run_tool(lua: &Lua, config: &HostConfig) -> Result<()> {
    let client = config.client.clone();
    let toolbox_dir = config.toolbox_dir.clone();
    let task_description = config.task_description.clone();
    let depth = config.recursion_depth.clone();
    let secrets = config.secrets.clone();
    let builtins = config.builtins.clone();
    let schedule_store = config.schedule_store.clone();
    let memory_store = config.memory_store.clone();
    let frontend_context = config.frontend_context.clone();

    let func =
        lua.create_async_function(move |lua, (name, params): (String, Option<mlua::Table>)| {
            let client = client.clone();
            let toolbox_dir = toolbox_dir.clone();
            let task_description = task_description.clone();
            let depth = depth.clone();
            let secrets = secrets.clone();
            let builtins = builtins.clone();
            let schedule_store = schedule_store.clone();
            let memory_store = memory_store.clone();
            let frontend_context = frontend_context.clone();
            async move {
                let current = depth.fetch_add(1, Ordering::SeqCst);
                if current >= MAX_RECURSION_DEPTH {
                    depth.fetch_sub(1, Ordering::SeqCst);
                    return Err(mlua::Error::external(format!(
                        "run_tool('{name}'): max recursion depth ({MAX_RECURSION_DEPTH}) exceeded"
                    )));
                }

                let inner_config = HostConfig {
                    client,
                    toolbox_dir,
                    task_description,
                    recursion_depth: depth.clone(),
                    secrets,
                    builtins,
                    schedule_store,
                    memory_store,
                    frontend_context,
                };

                let result = run_tool_inner(&name, params.as_ref(), &inner_config).await;

                depth.fetch_sub(1, Ordering::SeqCst);

                let json_value = result
                    .map_err(|e| mlua::Error::external(format!("run_tool('{name}'): {e}")))?;

                lua.to_value(&json_value)
            }
        })?;
    lua.globals().set("run_tool", func)?;
    Ok(())
}

fn register_builtin_tools(lua: &Lua, config: &HostConfig) -> Result<()> {
    for (name, tool) in config.builtins.as_ref() {
        let tool = tool.clone();
        let client = config.client.clone();
        let secrets = config.secrets.clone();
        let task_desc = config.task_description.clone();
        let schedule_store = config.schedule_store.clone();
        let memory_store = config.memory_store.clone();
        let frontend_context = config.frontend_context.clone();
        let tool_name = name.clone();

        let func = lua.create_async_function(move |lua, params: Option<mlua::Table>| {
            let tool = tool.clone();
            let client = client.clone();
            let secrets = secrets.clone();
            let task_desc = task_desc.clone();
            let schedule_store = schedule_store.clone();
            let memory_store = memory_store.clone();
            let frontend_context = frontend_context.clone();
            let tool_name = tool_name.clone();
            async move {
                let params_map = lua_params_to_hashmap(params.as_ref())
                    .map_err(|e| mlua::Error::external(format!("{tool_name}: {e}")))?;
                let secrets_obj = secrets_map_to_secrets(&secrets);
                let resolved = secrets_obj.resolve_params(&params_map);
                let ctx = ToolContext {
                    client,
                    secrets: Arc::new(secrets_obj),
                    task_description: task_desc,
                    schedule_store,
                    memory_store,
                    frontend_context,
                };
                let result = tool
                    .execute(resolved, ctx)
                    .await
                    .map_err(|e| mlua::Error::external(format!("{tool_name}: {e}")))?;
                lua.to_value(&result)
            }
        })?;
        lua.globals().set(name.as_str(), func)?;
    }
    Ok(())
}

fn register_toolbox_tools(lua: &Lua, config: &HostConfig) -> Result<()> {
    let toolbox_dir = match config.toolbox_dir {
        Some(ref dir) => dir,
        None => return Ok(()),
    };

    let entries = match std::fs::read_dir(toolbox_dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        if config.builtins.contains_key(&name) {
            continue;
        }

        let lua_path = toolbox_dir.join(format!("{name}.lua"));
        if !lua_path.exists() {
            continue;
        }

        let client = config.client.clone();
        let tb_dir = toolbox_dir.clone();
        let task_desc = config.task_description.clone();
        let depth = config.recursion_depth.clone();
        let secrets = config.secrets.clone();
        let builtins = config.builtins.clone();
        let schedule_store = config.schedule_store.clone();
        let memory_store = config.memory_store.clone();
        let frontend_context = config.frontend_context.clone();
        let tool_name = name.clone();

        let func = lua.create_async_function(move |lua, params: Option<mlua::Table>| {
            let client = client.clone();
            let tb_dir = tb_dir.clone();
            let task_desc = task_desc.clone();
            let depth = depth.clone();
            let secrets = secrets.clone();
            let builtins = builtins.clone();
            let schedule_store = schedule_store.clone();
            let memory_store = memory_store.clone();
            let frontend_context = frontend_context.clone();
            let tool_name = tool_name.clone();
            async move {
                let current = depth.fetch_add(1, Ordering::SeqCst);
                if current >= MAX_RECURSION_DEPTH {
                    depth.fetch_sub(1, Ordering::SeqCst);
                    return Err(mlua::Error::external(format!(
                        "{tool_name}: max recursion depth ({MAX_RECURSION_DEPTH}) exceeded"
                    )));
                }

                let inner_config = HostConfig {
                    client,
                    toolbox_dir: Some(tb_dir),
                    task_description: task_desc,
                    recursion_depth: depth.clone(),
                    secrets,
                    builtins,
                    schedule_store,
                    memory_store,
                    frontend_context,
                };

                let result = run_tool_inner(&tool_name, params.as_ref(), &inner_config).await;
                depth.fetch_sub(1, Ordering::SeqCst);

                let json_value =
                    result.map_err(|e| mlua::Error::external(format!("{tool_name}: {e}")))?;
                lua.to_value(&json_value)
            }
        })?;
        let global_name = format!("{TOOLBOX_PREFIX}{name}");
        lua.globals().set(global_name.as_str(), func)?;
    }
    Ok(())
}

async fn run_tool_inner(
    name: &str,
    params: Option<&mlua::Table>,
    config: &HostConfig,
) -> std::result::Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(tool) = config.builtins.get(name) {
        let params_map = lua_params_to_hashmap(params)?;
        let secrets_obj = secrets_map_to_secrets(&config.secrets);
        let resolved = secrets_obj.resolve_params(&params_map);
        let ctx = ToolContext {
            client: config.client.clone(),
            secrets: Arc::new(secrets_obj),
            task_description: config.task_description.clone(),
            schedule_store: config.schedule_store.clone(),
            memory_store: config.memory_store.clone(),
            frontend_context: config.frontend_context.clone(),
        };
        return tool.execute(resolved, ctx).await;
    }

    let toolbox_dir = config
        .toolbox_dir
        .as_ref()
        .ok_or_else(|| format!("tool '{name}' not found (no toolbox configured)"))?;
    let lua_path = toolbox_dir.join(format!("{name}.lua"));
    let source = std::fs::read_to_string(&lua_path)
        .map_err(|e| format!("failed to load tool '{name}': {e}"))?;

    let inner_lua = create_sandbox()?;

    let inner_config = HostConfig {
        client: config.client.clone(),
        toolbox_dir: Some(toolbox_dir.to_path_buf()),
        task_description: config.task_description.clone(),
        recursion_depth: config.recursion_depth.clone(),
        secrets: config.secrets.clone(),
        builtins: config.builtins.clone(),
        schedule_store: config.schedule_store.clone(),
        memory_store: config.memory_store.clone(),
        frontend_context: config.frontend_context.clone(),
    };
    register_host_functions(&inner_lua, &inner_config)?;

    let task_table = inner_lua.create_table()?;
    task_table.set("description", config.task_description.clone())?;
    inner_lua.globals().set("TASK", task_table)?;

    let params_table = inner_lua.create_table()?;
    if let Some(p) = params {
        for pair in p.pairs::<String, mlua::Value>() {
            let (k, v) = pair?;
            params_table.set(
                k,
                inner_lua.to_value(&inner_lua.from_value::<serde_json::Value>(v)?)?,
            )?;
        }
    }
    inner_lua.globals().set("PARAMS", params_table)?;

    let result: mlua::Value = inner_lua.load(&source).eval_async().await?;
    let json: serde_json::Value = inner_lua.from_value(result)?;

    Ok(json)
}

fn lua_params_to_hashmap(
    params: Option<&mlua::Table>,
) -> std::result::Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut map = HashMap::new();
    if let Some(p) = params {
        for pair in p.pairs::<String, mlua::Value>() {
            let (k, v) = pair?;
            let s = match v {
                Value::String(s) => s.to_str()?.to_string(),
                Value::Integer(n) => n.to_string(),
                Value::Number(n) => n.to_string(),
                Value::Boolean(b) => b.to_string(),
                _ => serde_json::to_string(&mlua::Lua::new().from_value::<serde_json::Value>(v)?)
                    .unwrap_or_default(),
            };
            map.insert(k, s);
        }
    }
    Ok(map)
}

fn secrets_map_to_secrets(map: &HashMap<String, String>) -> Secrets {
    Secrets::from_map(map.clone())
}

fn register_http_request(lua: &Lua, client: Arc<Client>) -> Result<()> {
    let func = lua.create_async_function(move |lua, opts: mlua::Table| {
        let client = client.clone();
        async move {
            let method: String = opts.get("method")?;
            let url: String = opts.get("url")?;
            let body: Option<String> = opts.get("body").ok();
            let headers: Option<mlua::Table> = opts.get("headers").ok();

            let method = method
                .parse::<reqwest::Method>()
                .map_err(|e| mlua::Error::external(format!("invalid HTTP method: {e}")))?;

            let mut req = client.request(method, &url);

            if let Some(h) = headers {
                for pair in h.pairs::<String, String>() {
                    let (k, v) = pair?;
                    req = req.header(k, v);
                }
            }

            if let Some(b) = body {
                req = req.body(b);
            }

            let resp = req.send().await.map_err(mlua::Error::external)?;

            let status = resp.status().as_u16();
            let resp_body = resp.text().await.map_err(mlua::Error::external)?;

            let result = lua.create_table()?;
            result.set("status", status)?;
            result.set("body", resp_body)?;
            Ok(Value::Table(result))
        }
    })?;
    lua.globals().set("http_request", func)?;
    Ok(())
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

fn register_xml_parse(lua: &Lua) -> Result<()> {
    let func = lua.create_function(|lua, input: String| {
        let node = crate::xml::parse(&input).map_err(mlua::Error::external)?;
        xml_node_to_lua(lua, &node)
    })?;
    lua.globals().set("xml_parse", func)?;
    Ok(())
}

fn register_xml_encode(lua: &Lua) -> Result<()> {
    let func = lua.create_function(|_, table: mlua::Table| {
        let node = lua_to_xml_node(&table)?;
        crate::xml::encode(&node).map_err(mlua::Error::external)
    })?;
    lua.globals().set("xml_encode", func)?;
    Ok(())
}

fn xml_node_to_lua(lua: &Lua, node: &XmlNode) -> Result<Value> {
    let table = lua.create_table()?;
    table.set("tag", node.tag.as_str())?;

    if !node.attrs.is_empty() {
        let attrs = lua.create_table()?;
        for (k, v) in &node.attrs {
            attrs.set(k.as_str(), v.as_str())?;
        }
        table.set("attrs", attrs)?;
    }

    if let Some(ref text) = node.text {
        table.set("text", text.as_str())?;
    }

    if !node.children.is_empty() {
        let children = lua.create_table()?;
        for (i, child) in node.children.iter().enumerate() {
            children.set(i + 1, xml_node_to_lua(lua, child)?)?;
        }
        table.set("children", children)?;
    }

    Ok(Value::Table(table))
}

fn lua_to_xml_node(table: &mlua::Table) -> Result<XmlNode> {
    let tag: String = table.get("tag")?;

    let mut attrs = HashMap::new();
    if let Ok(attrs_table) = table.get::<mlua::Table>("attrs") {
        for pair in attrs_table.pairs::<String, String>() {
            let (k, v) = pair?;
            attrs.insert(k, v);
        }
    }

    let text: Option<String> = table.get("text").ok();

    let mut children = Vec::new();
    if let Ok(children_table) = table.get::<mlua::Table>("children") {
        for pair in children_table.pairs::<i64, mlua::Table>() {
            let (_, child_table) = pair?;
            children.push(lua_to_xml_node(&child_table)?);
        }
    }

    Ok(XmlNode {
        tag,
        attrs,
        text,
        children,
    })
}

fn register_log(lua: &Lua) -> Result<()> {
    let func = lua.create_function(|_, msg: String| {
        eprintln!("[lua] {msg}");
        Ok(())
    })?;
    lua.globals().set("log", func)?;
    Ok(())
}

fn register_secret(lua: &Lua, secrets: Arc<HashMap<String, String>>) -> Result<()> {
    let func = lua.create_function(move |_, name: String| {
        secrets
            .get(&name)
            .cloned()
            .ok_or_else(|| mlua::Error::external(format!("secret '{name}' not found")))
    })?;
    lua.globals().set("secret", func)?;
    Ok(())
}
