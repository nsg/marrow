use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use reqwest::Client;
use serenity::async_trait;
use serenity::model::channel::Message as DiscordMessage;
use serenity::model::gateway::GatewayIntents;
use serenity::model::gateway::Ready;
use serenity::model::id::ChannelId;
use serenity::prelude::*;

use tokio::sync::{RwLock, mpsc};

use marrow::agent;
use marrow::agent::ProgressTx;
use marrow::events::{Event, EventLog};
use marrow::janitor;
use marrow::memory::MemoryStore;
use marrow::memory_writer;
use marrow::router::{ModelRouter, RouterConfig};
use marrow::secrets::Secrets;
use marrow::session::{ChatSession, Message};
use marrow::toolbox::Toolbox;

// ---------------------------------------------------------------------------
// Shared state stored in serenity's TypeMap
// ---------------------------------------------------------------------------

struct RouterKey;
impl TypeMapKey for RouterKey {
    type Value = Arc<ModelRouter>;
}

struct ToolboxKey;
impl TypeMapKey for ToolboxKey {
    type Value = Arc<Toolbox>;
}

struct ToolboxPathKey;
impl TypeMapKey for ToolboxPathKey {
    type Value = String;
}

struct MemoryKey;
impl TypeMapKey for MemoryKey {
    type Value = Arc<MemoryStore>;
}

struct HttpClientKey;
impl TypeMapKey for HttpClientKey {
    type Value = Arc<Client>;
}

struct EventLogKey;
impl TypeMapKey for EventLogKey {
    type Value = Arc<EventLog>;
}

struct ChannelsKey;
impl TypeMapKey for ChannelsKey {
    type Value = Arc<Vec<u64>>;
}

struct SecretsKey;
impl TypeMapKey for SecretsKey {
    type Value = Arc<Secrets>;
}

struct SessionsKey;
impl TypeMapKey for SessionsKey {
    type Value = Arc<RwLock<HashMap<ChannelId, ChatSession>>>;
}

// ---------------------------------------------------------------------------
// Event handler
// ---------------------------------------------------------------------------

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        eprintln!("[marrow-discord] connected as {}", ready.user.name);
    }

    async fn message(&self, ctx: Context, msg: DiscordMessage) {
        // Ignore messages from bots (including ourselves)
        if msg.author.bot {
            return;
        }

        // Respond in DMs, when @mentioned, or in configured channels
        let is_dm = msg.guild_id.is_none();
        let is_mentioned = msg.mentions_me(&ctx.http).await.unwrap_or(false);
        let is_watched_channel = {
            let data = ctx.data.read().await;
            let channels = data.get::<ChannelsKey>().unwrap();
            channels.contains(&msg.channel_id.get())
        };

        if !is_dm && !is_mentioned && !is_watched_channel {
            return;
        }

        // Strip the bot mention from the message content
        let content = msg.content.trim();
        if content.is_empty() {
            return;
        }

        // Extract shared state
        let data = ctx.data.read().await;
        let router = data.get::<RouterKey>().unwrap().clone();
        let toolbox = data.get::<ToolboxKey>().unwrap().clone();
        let toolbox_path = data.get::<ToolboxPathKey>().unwrap().clone();
        let memory_store = data.get::<MemoryKey>().unwrap().clone();
        let client = data.get::<HttpClientKey>().unwrap().clone();
        let log = data.get::<EventLogKey>().unwrap().clone();
        let secrets = data.get::<SecretsKey>().unwrap().clone();
        let sessions = data.get::<SessionsKey>().unwrap().clone();
        drop(data);

        // Get conversation history for this channel
        let conversation = {
            let sessions_map = sessions.read().await;
            sessions_map
                .get(&msg.channel_id)
                .map(|s| s.build_messages(None))
                .unwrap_or_default()
        };

        // Show typing indicator while processing
        let typing = msg.channel_id.start_typing(&ctx.http);

        // Progress channel — agent sends updates, we forward to Discord
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
        let channel_id = msg.channel_id;
        let http = ctx.http.clone();
        let progress_handle = tokio::spawn(async move {
            while let Some(status) = progress_rx.recv().await {
                let _ = channel_id.say(&http, &status).await;
            }
        });

        // Run the agent
        let response = match run_task(
            content,
            &router,
            &toolbox,
            &toolbox_path,
            &memory_store,
            client,
            &log,
            &secrets,
            &progress_tx,
            &conversation,
        )
        .await
        {
            Ok(output) => output.as_str().unwrap_or("").to_string(),
            Err(e) => format!("Error: {e}"),
        };

        // Close channel and wait for remaining progress messages
        drop(progress_tx);
        let _ = progress_handle.await;
        drop(typing);

        // Send response, splitting if it exceeds Discord's 2000 char limit
        for chunk in split_message(&response, 2000) {
            if let Err(e) = msg.channel_id.say(&ctx.http, chunk).await {
                eprintln!("[marrow-discord] failed to send message: {e}");
            }
        }

        // Track conversation history per channel
        {
            let mut sessions_map = sessions.write().await;
            let session = sessions_map
                .entry(msg.channel_id)
                .or_insert_with(ChatSession::new);
            session.append(Message::user(content));
            session.append(Message::assistant(&response));

            if session.needs_summarization()
                && let Ok(backend) = router
                    .backend("fast")
                    .or_else(|_| router.backend("default"))
                && let Err(e) = session.summarize(backend).await
            {
                eprintln!("[marrow-discord] session summarize error: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Agent task runner (mirrors marrow-cli's run_task)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_task(
    description: &str,
    router: &ModelRouter,
    toolbox: &Toolbox,
    toolbox_path: &str,
    memory_store: &MemoryStore,
    client: Arc<Client>,
    log: &EventLog,
    secrets: &Secrets,
    progress: &ProgressTx,
    conversation: &[Message],
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let task_id = uuid::Uuid::new_v4().to_string();

    log.emit(Event::TaskCreated {
        task_id: task_id.clone(),
        description: description.to_string(),
        role: "discord".to_string(),
    })
    .await;

    let fast_backend = router
        .backend("fast")
        .or_else(|_| router.backend("default"))?;
    let answer_backend = router
        .backend("default")
        .or_else(|_| router.backend("fast"))?;
    let code_backend = router
        .backend("code")
        .or_else(|_| router.backend("default"))?;

    let memories = memory_store.list().unwrap_or_default();

    let answer = agent::run_loop(
        description,
        &task_id,
        fast_backend,
        answer_backend,
        code_backend,
        toolbox,
        toolbox_path,
        client,
        &memories,
        log,
        Some(secrets),
        Some(progress),
        conversation,
    )
    .await?;

    log.emit(Event::TaskExecuted {
        task_id: task_id.clone(),
        status: "succeeded".to_string(),
    })
    .await;

    match memory_writer::process_interaction(description, &answer, memory_store, fast_backend).await
    {
        Ok(result) => {
            for fact in &result.saved {
                let _ = progress.send(format!("🧠 Remembered: {fact}"));
            }
            for fact in &result.updated {
                let _ = progress.send(format!("🧠 Updated: {fact}"));
            }
            if result.deleted > 0 {
                let _ = progress.send(format!("🧠 Forgot {} fact(s)", result.deleted));
            }
        }
        Err(e) => eprintln!("[marrow-discord] memory writer error: {e}"),
    }

    Ok(serde_json::Value::String(answer))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn split_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = (start + max_len).min(text.len());
        // Try to split at a newline or space boundary
        let split_at = if end == text.len() {
            end
        } else {
            text[start..end]
                .rfind('\n')
                .or_else(|| text[start..end].rfind(' '))
                .map(|i| start + i + 1)
                .unwrap_or(end)
        };
        chunks.push(&text[start..split_at]);
        start = split_at;
    }
    chunks
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = RouterConfig::from_file("config.toml")?;
    let discord = config
        .discord
        .as_ref()
        .ok_or("[discord] section missing from config.toml")?;

    let token = discord
        .token
        .as_deref()
        .ok_or("[discord] token missing from config.toml")?;
    let toolbox_path = discord
        .toolbox
        .clone()
        .unwrap_or_else(|| "toolbox".to_string());
    let memory_path = discord
        .memory
        .clone()
        .unwrap_or_else(|| "memory".to_string());
    let log_path = discord
        .log
        .clone()
        .unwrap_or_else(|| "events.jsonl".to_string());
    let verbose = discord.verbose;
    let router = Arc::new(ModelRouter::from_config(&config)?);
    let client = Arc::new(Client::new());
    let toolbox = Arc::new(Toolbox::new(&toolbox_path));
    let memory_store = Arc::new(MemoryStore::new(&memory_path));
    let log = Arc::new(EventLog::new(Some(PathBuf::from(&log_path)), verbose).await?);
    let secrets = Arc::new(Secrets::load_or_empty("secrets.toml"));

    // Spawn janitor in background
    let janitor_backend = config
        .build_backend("code")
        .or_else(|_| config.build_backend("default"))?;
    let janitor_toolbox = Toolbox::new(&toolbox_path);
    let janitor_log = log.clone();
    tokio::spawn(async move {
        janitor::run(&janitor_toolbox, janitor_backend.as_ref(), &janitor_log).await;
    });

    // Discord gateway intents — we need message content to read user messages
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;

    let mut discord_client = serenity::Client::builder(token, intents)
        .event_handler(Handler)
        .await?;

    let channels = Arc::new(discord.channels.clone());

    // Store shared state
    {
        let mut data = discord_client.data.write().await;
        data.insert::<RouterKey>(router);
        data.insert::<ToolboxKey>(toolbox);
        data.insert::<ToolboxPathKey>(toolbox_path);
        data.insert::<MemoryKey>(memory_store);
        data.insert::<HttpClientKey>(client);
        data.insert::<EventLogKey>(log);
        data.insert::<ChannelsKey>(channels);
        data.insert::<SecretsKey>(secrets);
        data.insert::<SessionsKey>(Arc::new(RwLock::new(HashMap::new())));
    }

    eprintln!("[marrow-discord] starting...");
    discord_client.start().await?;

    Ok(())
}
