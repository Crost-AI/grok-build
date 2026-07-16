# Discord channel plugin

Message your Grok Build session from Discord. This plugin is a [channel](../../crates/codegen/xai-grok-pager/docs/user-guide/25-channels.md): it forwards Discord messages (DMs, or guild messages that @mention the bot) into a **running** session, and gives the agent tools to reply, react, and read channel history — so you can kick off and steer work from your phone while the session runs against your real files.

```text
you, on Discord:            @grok why is CI red on main?
your running session:       <channel source="discord" channel_id="1234..." author="karl" ...>
                            why is CI red on main?
                            </channel>
agent → send_message tool:  Two tests in auth_flow are failing after #482 — bisecting now.
you, on Discord:            (the reply arrives in the same Discord channel)
```

## Setup

### 1. Create a Discord bot

1. Open the [Discord developer portal](https://discord.com/developers/applications) → **New Application**.
2. Under **Bot**, click **Reset Token** and copy the token — this is `DISCORD_BOT_TOKEN`.
3. Still under **Bot → Privileged Gateway Intents**, enable **Message Content Intent** (without it, guild messages arrive with empty text and are dropped).
4. Invite the bot to your server: **OAuth2 → URL Generator**, scope `bot`, permissions **View Channels**, **Send Messages**, **Read Message History**, **Add Reactions**; open the generated URL. (DMs also require sharing at least one server with the bot.)

### 2. Find your Discord user id

Discord **Settings → Advanced → Developer Mode** on, then right-click your own avatar → **Copy User ID**. Only ids you allowlist can push messages into your session.

### 3. Install the plugin

```sh
grok plugin marketplace add lakerfan901/grok-build
grok plugin install discord@grok-build --trust
```

The `@grok-build` qualifier matters: other configured marketplaces (including the default official one) may also carry a plugin named `discord`, and an unqualified install is rejected as ambiguous. (Installing straight from the repo also works: `grok plugin install lakerfan901/grok-build#plugins/discord --trust`.)

### 4. Configure credentials

Channel credentials live outside your project, in `~/.grok/channels/<server>/.env` — Grok injects them into the server's environment only when the session opts the channel in:

```sh
mkdir -p ~/.grok/channels/discord
cat > ~/.grok/channels/discord/.env <<'EOF'
DISCORD_BOT_TOKEN=your-bot-token
DISCORD_ALLOWED_USER_IDS=your-user-id
EOF
chmod 600 ~/.grok/channels/discord/.env
```

### 5. Start a session with the channel enabled

```sh
grok --channels plugin:discord@grok-build
```

Run `/channels` in the session — it should show the entry as `active` with the `discord` server connected and the channel capability declared. Then DM your bot (or @mention it in a channel it can see).

## Configuration reference

All via environment variables (put them in the `.env` file above):

| Variable | Required | Default | Meaning |
|----------|----------|---------|---------|
| `DISCORD_BOT_TOKEN` | yes | — | Bot token from the developer portal |
| `DISCORD_ALLOWED_USER_IDS` | to receive | *(none — everything dropped)* | Comma-separated Discord user ids allowed to push messages into the session. `*` allows everyone (not recommended) |
| `DISCORD_CHANNEL_IDS` | no | all visible channels | Comma-separated guild channel ids to listen to; others are ignored (does not affect DMs) |
| `DISCORD_ALLOW_DMS` | no | `true` | Set `false` to ignore direct messages |
| `DISCORD_REQUIRE_MENTION` | no | `true` | Set `false` to forward guild messages that don't @mention the bot |

## What the agent can do

- **`send_message`** — post to a channel (used to reply; content over Discord's 2000-character limit is split automatically, and it can attach the reply to a specific message).
- **`add_reaction`** — emoji-react to a message (a cheap "on it 👍" acknowledgement).
- **`read_messages`** — fetch recent history from a channel for context that wasn't forwarded.

Inbound events follow standard channel delivery: an idle session wakes immediately; a busy session receives queued events together at the end of the current turn.

## Security

- **Sender allowlist is mandatory.** With `DISCORD_ALLOWED_USER_IDS` unset, every inbound message is dropped (and the server logs why). The gate is on the *sender's id*, not the room — anyone else in an allowed channel is still ignored.
- Everything arriving over the channel is untrusted input in front of your agent. Prefer keeping permission prompts on (or a sandbox) for channel-driven sessions; `--always-approve` plus an open channel means allowlisted senders operate a fully-approved agent.
- The bot token grants control of the bot account — keep the `.env` file at mode `600` and out of version control.

## Development

The bridge is a single zero-dependency Node (≥ 22) script: [`server/discord-channel.mjs`](server/discord-channel.mjs). It speaks MCP over stdio, declares the `grok/channel` capability, maintains the Discord gateway connection (identify, heartbeat, resume/reconnect), and pushes gated messages as `notifications/grok/channel`.

Run the end-to-end test (spawns the bridge against a mock Discord gateway and REST API — no network, no token needed):

```sh
node plugins/discord/server/discord-channel.test.mjs
```

It is also a working reference for building your own channel plugin — see [Building a Channel](../../crates/codegen/xai-grok-pager/docs/user-guide/25-channels.md#building-a-channel).
