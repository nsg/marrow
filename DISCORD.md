# Discord Bot

Marrow connects to Discord as a bot that responds to @mentions and DMs. Setup takes about 5 minutes.

## Create the bot

1. Open the [Discord Developer Portal](https://discord.com/developers/applications) and click **New Application**
2. Name it (e.g. "Marrow") and click **Create**

**Make the bot private** (optional but recommended for personal use):

3. Go to **Installation** in the left sidebar
4. Set the Install Link to **None** and save
5. Go to **Bot** in the left sidebar and uncheck **Public Bot**

   You have to disable the install link first — otherwise Discord blocks it with a confusing "Cannot have install fields on a private application" error. With Public Bot off, only you can add the bot to servers.

**Enable intents and grab your token:**

6. Still on the **Bot** page, scroll to **Privileged Gateway Intents** and turn on **Message Content Intent** — this is required; without it, the bot receives empty messages. Leave the other two intents off (not needed).
7. Click **Reset Token** and copy the token. Despite the name, this generates your first token — you only see it once, so save it now.

> Your bot token is a secret. Do not commit it to version control or share it in chat.

## Invite the bot to your server

1. Go to **OAuth2** in the left sidebar, then scroll to **OAuth2 URL Generator**
2. Under **Scopes**, tick `bot`
3. Under **Bot Permissions**, tick:
   - **View Channels** (under General Permissions)
   - **Send Messages** (under Text Permissions)
   - **Read Message History** (under Text Permissions)

   Do not tick Administrator — the bot only needs these three.

4. Copy the generated URL at the bottom, open it in your browser, and pick the server to add the bot to

## Configure

Add a `[discord]` section to your `config.toml` with at least `token = "..."`. See the commented `[discord]` block in [`config.example.toml`](config.example.toml) for all available fields (`token`, `channels`, `toolbox`, `memory`, `log`, `verbose`).

By default the bot only responds when @mentioned or in DMs. To have it respond to every message in specific channels, list those channel IDs in `channels = [...]`. To get a channel ID, enable **Developer Mode** in Discord settings (App Settings > Advanced), then right-click a channel and click **Copy Channel ID**.
