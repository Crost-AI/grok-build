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
4. Invite the bot to your server: **OAuth2 → URL Generator**, scope `bot`, permissions **View Channels**, **Send Messages**, **Read Message History**, **Add Reactions**, **Attach Files** (for `send_file`), **Create Public Threads** (for `create_thread`), and optionally **Manage Threads** (to rename/close threads the bot didn't create, and to lock threads); open the generated URL. (DMs also require sharing at least one server with the bot.)

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
| `DISCORD_ALLOWED_BOT_IDS` | no | *(bots ignored)* | Comma-separated bot user ids allowed to trigger the session. Bots are ignored by default; see [Listening to other bots](#listening-to-other-bots) |
| `DISCORD_CHANNEL_IDS` | no | all visible channels | Comma-separated guild channel ids to listen to; others are ignored (does not affect DMs) |
| `DISCORD_ALLOW_DMS` | no | `true` | Set `false` to ignore direct messages |
| `DISCORD_REQUIRE_MENTION` | no | `true` | Set `false` to forward guild messages that don't @mention the bot |
| `DISCORD_MENTION_WINDOW_SECONDS` | no | `60` | After a sender's message is forwarded, their follow-ups in the same channel pass the mention gate for this long (sliding). Covers content split by the 2000-char limit — only the first chunk carries the mention — and quick follow-ups. `0` disables |

With the mention requirement on, a guild message is treated as addressed to the bot if it @mentions the bot (user **or** its managed role), is a Discord **reply** to one of the bot's messages, or falls inside the sender's continuation window above. Sender allowlists always apply regardless.

**Listening without the mention requirement** (`DISCORD_REQUIRE_MENTION=false`): every allowed sender's message in allowed channels flows in, and messages not directed at the bot carry an `addressed` attribute — `addressed="other"` (someone else was mentioned or replied to) or `addressed="none"` (open chatter). The agent is instructed to read those for context but stay silent unless it can correct a clear factual error or something urgent needs attention. Pair this mode with `DISCORD_CHANNEL_IDS` so the bot only listens where it's wanted.

## What the agent can do

- **`send_message`** — post to a channel (used to reply; content over Discord's 2000-character limit is split automatically, and it can attach the reply to a specific message).
- **`add_reaction`** — emoji-react to a message (a cheap "on it 👍" acknowledgement).
- **`read_messages`** — fetch recent history from a channel for context that wasn't forwarded.
- **`read_poll`** / **`end_poll`** — read a poll's standings (per-answer counts and up to 25 voter names) and end one of the bot's own polls early. Note: **Discord does not let bots cast native poll votes** — when an agent is asked to vote, it replies in the channel stating its choice; humans vote natively and `read_poll` tallies them.
- **`create_poll`** — post a native Discord poll (`POST /channels/{id}/messages` with a `poll` body). Question ≤300 chars; 2–10 answers ≤55 chars each; `duration` is hours (1–768, default 24); optional caption, multiselect, and per-answer emoji. Poll messages cannot be edited after posting.
- **`create_thread`** — open a public workstream thread under an allowlisted parent text channel (`POST /channels/{id}/threads`, type 11). Name is sanitized (mentions stripped, max 100 chars); **24h auto-archive**; optional first message. Parent must be in `DISCORD_CHANNEL_IDS` when that allowlist is set. New threads inherit parent allowlist for inbound (v0.1.5+).
- **`rename_thread`** / **`close_thread`** — retitle a thread as the work evolves, or archive it (optionally lock it) when the workstream wraps up. Both verify the target really is a thread — they can never touch a regular channel — and respect the `DISCORD_CHANNEL_IDS` parent allowlist. The bot can rename/close threads it created; other threads (and locking) need the **Manage Threads** permission on the bot.
- **`send_file`** — upload a file from the machine as a Discord attachment (10 MB bot limit), with an optional caption. Requires the **Attach Files** permission on the bot (see the invite step).
- **`read_attachment`** — download an incoming attachment (the `[attachment ...: url]` lines in forwarded messages) to a temp file under the OS temp dir and return its path, so the agent can read images, logs, or documents you drop in the chat. Restricted to Discord's CDN hosts; 25 MB cap; CDN links expire after a while, so old attachments may need re-sending.

Inbound events follow standard channel delivery: an idle session wakes immediately; a busy session receives queued events together at the end of the current turn.

## Slash commands from Discord

A few session commands work straight from Discord — the **host** executes them and replies in the channel; the agent never sees them (and they don't interrupt whatever it's doing):

| You type | You get back |
|----------|--------------|
| `/status` (or `/session`) | Session id, model, working directory, turn, context usage |
| `/channels` | The session's channel entries with live per-server status |
| `/help` | The list above |

With the mention requirement on, address the bot as usual: `@grok /status`. The mention is stripped before the command is parsed. Command messages must *start* with the `/` (after the mention); anything else — including `/commands` the host doesn't recognize, like skill invocations — is forwarded to the agent as a normal message. Bot-authored messages are never treated as commands, so another agent can't drive your session's host commands; what a bot sends always goes to the agent to judge.

## Listening to other bots

By default the bridge ignores every bot-authored message. To let another agent's bot (say, a Claude Code Discord bot in the same server) talk to your session, add its bot user id (Developer Mode → right-click the bot → Copy User ID) to `DISCORD_ALLOWED_BOT_IDS`. Its messages then pass the sender gate and arrive marked `bot="true"`, and the agent is instructed to keep bot-to-bot replies terse and purposeful.

Practical setup for two coordinating agents: a dedicated channel, `DISCORD_CHANNEL_IDS=<that channel>`, and `DISCORD_REQUIRE_MENTION=false` — bots rarely emit *real* mention entities (plain-text "@grok" in a bot's message is not a mention and won't pass the mention gate), so a mention requirement usually silences them.

**Mind the loop.** Two agents that each respond to the other can ping-pong forever, burning tokens on both sides. Safest is one-directional listening (your session hears the other bot, but the other bot doesn't allowlist yours). If you enable both directions, make sure at least one side replies only when addressed or has its own loop guard.

## Troubleshooting

- **`/channels` says the server "does NOT declare the grok/channel capability"** or the entry is blocked with a marketplace mismatch: another plugin named `discord` (for example Claude Code's Discord plugin, discovered through the `~/.claude/plugins` compat layer) is shadowing this one. Grok-native installs take precedence over compat-discovered plugins, so `grok plugin install discord@grok-build --trust` normally wins; check `grok plugin details discord` to see which plugin is active and where it came from.
- **Messages are silently ignored**: the sender's id isn't in `DISCORD_ALLOWED_USER_IDS`, or you forgot to @mention the bot in a server channel. The bridge logs every drop reason to stderr (visible in Grok's MCP logs).
- **Guild messages arrive empty / `4014 disallowed intents` in the logs**: enable **Message Content Intent** in the developer portal (Bot → Privileged Gateway Intents) and save.

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
