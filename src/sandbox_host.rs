use mlua::{Lua, LuaSerdeExt, Result, Value};
use reqwest::Client;
use std::sync::Arc;

pub fn register_host_functions(lua: &Lua, client: Arc<Client>) -> Result<()> {
    register_http_get(lua, client.clone())?;
    register_http_post(lua, client)?;
    register_json_parse(lua)?;
    register_json_encode(lua)?;
    register_log(lua)?;
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

fn register_log(lua: &Lua) -> Result<()> {
    let func = lua.create_function(|_, msg: String| {
        eprintln!("[lua] {msg}");
        Ok(())
    })?;
    lua.globals().set("log", func)?;
    Ok(())
}
