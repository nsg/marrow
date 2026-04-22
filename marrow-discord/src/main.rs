use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use serenity::async_trait;
use serenity::model::channel::Message as DiscordMessage;
use serenity::model::gateway::GatewayIntents;
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId};
use serenity::prelude::*;

use tokio::sync::{RwLock, mpsc};

use marrow::agent::IncomingRx;
use marrow::heartbeat;
use marrow::router::RouterConfig;
use marrow::runtime::{Runtime, RuntimeOptions};
use marrow::session::{ChatSession, Message};
use marrow::tool::FrontendContext;

// ---------------------------------------------------------------------------
// Shared state stored in serenity's TypeMap
// ---------------------------------------------------------------------------

struct RuntimeKey;
impl TypeMapKey for RuntimeKey {
    type Value = Arc<Runtime>;
}

struct ChannelsKey;
impl TypeMapKey for ChannelsKey {
    type Value = Arc<Vec<u64>>;
}

struct SessionsKey;
impl TypeMapKey for SessionsKey {
    type Value = Arc<RwLock<HashMap<ChannelId, ChatSession>>>;
}

struct ActiveTasksKey;
impl TypeMapKey for ActiveTasksKey {
    type Value = Arc<RwLock<HashMap<ChannelId, mpsc::UnboundedSender<String>>>>;
}

struct SeenMessagesKey;
impl TypeMapKey for SeenMessagesKey {
    type Value = Arc<RwLock<Vec<MessageId>>>;
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

        // Deduplicate — Discord gateway can deliver the same message event twice
        {
            let data = ctx.data.read().await;
            let seen = data.get::<SeenMessagesKey>().unwrap().clone();
            drop(data);
            let mut seen = seen.write().await;
            if seen.contains(&msg.id) {
                return;
            }
            seen.push(msg.id);
            if seen.len() > 256 {
                let excess = seen.len() - 256;
                seen.drain(..excess);
            }
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

        // If a task is already running in this channel, forward as interjection
        {
            let data = ctx.data.read().await;
            let active = data.get::<ActiveTasksKey>().unwrap().clone();
            drop(data);
            let active_map = active.read().await;
            if let Some(tx) = active_map.get(&msg.channel_id) {
                let _ = tx.send(content.to_string());
                let _ = msg.react(&ctx.http, '👂').await;
                return;
            }
        }

        // Extract shared state
        let data = ctx.data.read().await;
        let runtime = data.get::<RuntimeKey>().unwrap().clone();
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
            let mut status_msg: Option<serenity::model::channel::Message> = None;
            while let Some(status) = progress_rx.recv().await {
                if let Some(ref mut msg) = status_msg {
                    let _ = msg
                        .edit(
                            &http,
                            serenity::builder::EditMessage::new().content(&status),
                        )
                        .await;
                } else {
                    if let Ok(m) = channel_id.say(&http, &status).await {
                        status_msg = Some(m);
                    }
                }
            }
            // Delete the status message when the task is done
            if let Some(msg) = status_msg {
                let _ = msg.delete(&http).await;
            }
        });

        // Incoming channel — user follow-ups forwarded to the agent loop
        let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<String>();
        {
            let data = ctx.data.read().await;
            let active = data.get::<ActiveTasksKey>().unwrap().clone();
            drop(data);
            let mut active_map = active.write().await;
            active_map.insert(msg.channel_id, incoming_tx);
        }

        // Run the agent
        let response = match run_task(
            content,
            runtime.as_ref(),
            &progress_tx,
            &conversation,
            &mut incoming_rx,
            msg.channel_id.get(),
        )
        .await
        {
            Ok(output) => output,
            Err(e) => format!("Error: {e}"),
        };

        // Unregister incoming channel and close progress
        {
            let data = ctx.data.read().await;
            let active = data.get::<ActiveTasksKey>().unwrap().clone();
            drop(data);
            let mut active_map = active.write().await;
            active_map.remove(&msg.channel_id);
        }
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
                && let Ok(backend) = runtime.fast_backend()
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

const DISCORD_FORMATTING_HINT: &str = "Formatting: The response will be displayed in Discord. Discord supports **bold**, *italic*, __underline__, ~~strikethrough~~, `inline code`, ```code blocks```, > quotes, and bullet lists (- item). Discord does NOT support markdown tables, headings (#), or horizontal rules. Keep responses concise — messages over 2000 characters are split.";

async fn run_task(
    description: &str,
    runtime: &Runtime,
    progress: &mpsc::UnboundedSender<String>,
    conversation: &[Message],
    incoming: &mut IncomingRx,
    channel_id: u64,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    runtime
        .run_task(
            description,
            "discord",
            conversation,
            Some(progress),
            Some(incoming),
            Some(DISCORD_FORMATTING_HINT),
            Some(FrontendContext {
                frontend: "discord".to_string(),
                channel_id: Some(channel_id),
            }),
        )
        .await
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
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "marrow-discord", about = "Marrow agent as a Discord bot")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Path to the toolbox directory (overrides config)
    #[arg(short, long)]
    toolbox: Option<String>,

    /// Path to the memory directory (overrides config)
    #[arg(short, long)]
    memory: Option<String>,

    /// Show full event stream
    #[arg(short, long)]
    verbose: bool,

    /// Path to event log file (overrides config)
    #[arg(long)]
    log: Option<String>,

    /// Check for updates and apply if available
    #[arg(long)]
    update: bool,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    if cli.update {
        match marrow::update::check_and_update().await {
            Ok(true) => {
                eprintln!("[marrow-discord] restart to use the new version");
                return Ok(());
            }
            Ok(false) => return Ok(()),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    let config = RouterConfig::from_file(&cli.config)?;
    let discord = config
        .discord
        .as_ref()
        .ok_or("[discord] section missing from config.toml")?;

    let token = discord
        .token
        .as_deref()
        .ok_or("[discord] token missing from config.toml")?;
    let toolbox_path = cli.toolbox.unwrap_or_else(|| {
        discord
            .toolbox
            .clone()
            .unwrap_or_else(|| "toolbox".to_string())
    });
    let memory_path = cli.memory.unwrap_or_else(|| {
        discord
            .memory
            .clone()
            .unwrap_or_else(|| "memory".to_string())
    });
    let log_path = cli.log.unwrap_or_else(|| {
        discord
            .log
            .clone()
            .unwrap_or_else(|| "events.jsonl".to_string())
    });
    let verbose = cli.verbose || discord.verbose;
    let scheduler_config = config.scheduler.as_ref();
    let schedule_path = scheduler_config
        .map(|s| s.schedules_path().to_string())
        .unwrap_or_else(|| "schedules".to_string());
    let tick_seconds = scheduler_config.map(|s| s.tick()).unwrap_or(60);
    let scheduler_enabled = scheduler_config.is_none_or(|s| s.enabled);

    let runtime = Arc::new(
        Runtime::from_config(
            &config,
            RuntimeOptions {
                toolbox_path,
                memory_path,
                log_path,
                verbose,
                secrets_path: "secrets.toml".to_string(),
                spawn_janitor: true,
                schedule_path,
            },
        )
        .await?,
    );

    // Discord gateway intents — we need message content to read user messages
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;

    let mut discord_client = serenity::Client::builder(token, intents)
        .event_handler(Handler)
        .await?;

    let channels = Arc::new(discord.channels.clone());

    // Spawn heartbeat scheduler (delivers results to Discord channels)
    if scheduler_enabled {
        let hb_runtime = runtime.clone();
        let hb_store = runtime.schedule_store().clone();
        let hb_log = runtime.log().clone();
        let http_for_schedule = discord_client.http.clone();
        let (schedule_tx, mut schedule_rx) = mpsc::unbounded_channel::<heartbeat::ScheduleResult>();

        // Result receiver — posts scheduled task answers to Discord
        tokio::spawn(async move {
            while let Some(result) = schedule_rx.recv().await {
                if let Some(cid) = result.channel_id {
                    let channel = ChannelId::new(cid);
                    for chunk in split_message(&result.answer, 2000) {
                        let _ = channel.say(&http_for_schedule, chunk).await;
                    }
                }
            }
        });

        // Heartbeat loop
        tokio::spawn(async move {
            heartbeat::run(hb_runtime, hb_store, hb_log, schedule_tx, tick_seconds).await;
        });
    }

    // Store shared state
    {
        let mut data = discord_client.data.write().await;
        data.insert::<RuntimeKey>(runtime);
        data.insert::<ChannelsKey>(channels);
        data.insert::<SessionsKey>(Arc::new(RwLock::new(HashMap::new())));
        data.insert::<ActiveTasksKey>(Arc::new(RwLock::new(HashMap::new())));
        data.insert::<SeenMessagesKey>(Arc::new(RwLock::new(Vec::new())));
    }

    eprintln!("[marrow-discord] starting...");
    discord_client.start().await?;

    Ok(())
}
