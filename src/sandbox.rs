use mlua::{Lua, Result, StdLib, Value};

const DISABLED_GLOBALS: &[&str] = &[
    "dofile",
    "loadfile",
    "load",
    "rawset",
    "rawget",
    "rawequal",
    "rawlen",
    "collectgarbage",
    "require",
];

pub fn create_sandbox() -> Result<Lua> {
    let libs = StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8;
    let lua = Lua::new_with(libs, Default::default())?;

    // Remove unsafe globals
    {
        let globals = lua.globals();
        for name in DISABLED_GLOBALS {
            globals.set(*name, Value::Nil)?;
        }

        // Remove debug library entirely
        globals.set("debug", Value::Nil)?;
        // Remove os library entirely
        globals.set("os", Value::Nil)?;
        // Remove io library entirely
        globals.set("io", Value::Nil)?;
        // Remove package/require system
        globals.set("package", Value::Nil)?;
    }

    Ok(lua)
}
