# Slack channel plugin

Message your Grok Build session from Slack. This plugin is a [channel](../../crates/codegen/xai-grok-pager/docs/user-guide/25-channels.md): it forwards Slack messages (DMs, or channel messages that @mention the bot) into a **running** session over Socket Mode — no public URL or webhook endpoint required — and gives the agent thread-aware tools to reply, react, and read history.

## Setup

### 1. Create a Slack app

1. Open [api.slack.com/apps](https://api.slack.com/apps) → **Create New App** → *From scratch*, pick your workspace.
2. **Socket Mode** (left sidebar) → enable it. When prompted, create an app-level token with the `connections:write` scope — the `xapp-...` value is your `SLACK_APP_TOKEN`.
3. **OAuth & Permissions → Bot Token Scopes**, add: `chat:write`, `reactions:write`, `channels:history`, `groups:history`, `im:history`, `users:read`, `files:write` (for `send_file`), `files:read` (for `read_attachment`).
4. **Event Subscriptions** → enable, and under *Subscribe to bot events* add: `message.channels`, `message.groups`, `message.im`. (Don't also add `app_mention` — the bridge ignores it to avoid double delivery.)
5. **Install App** → install to your workspace → copy the **Bot User OAuth Token** (`xoxb-...`) — your `SLACK_BOT_TOKEN`.
6. Invite the bot to the channels it should hear: `/invite @grok` in each.

### 2. Find your Slack member id

Your profile → **⋯** → **Copy member ID** (a `U...` value). Only allowlisted member ids can push messages into your session.

### 3. Install the plugin

```sh
grok plugin marketplace add lakerfan901/grok-build
grok plugin install slack@grok-build --trust
```

### 4. Configure credentials

```sh
mkdir -p ~/.grok/channels/slack
cat > ~/.grok/channels/slack/.env <<'EOF'
SLACK_BOT_TOKEN=xoxb-your-bot-token
SLACK_APP_TOKEN=xapp-your-app-token
SLACK_ALLOWED_USER_IDS=U-your-member-id
EOF
chmod 600 ~/.grok/channels/slack/.env
```

### 5. Start a session with the channel enabled

```sh
grok --channels plugin:slack@grok-build
```

`/channels` should show the entry `active` with the `slack` server connected and the channel capability declared. DM the bot, or mention it in a channel it's been invited to.

## Configuration reference

| Variable | Required | Default | Meaning |
|----------|----------|---------|---------|
| `SLACK_BOT_TOKEN` | yes | — | Bot user OAuth token (`xoxb-...`) |
| `SLACK_APP_TOKEN` | yes | — | App-level token with `connections:write` (`xapp-...`) for Socket Mode |
| `SLACK_ALLOWED_USER_IDS` | to receive | *(none — everything dropped)* | Comma-separated Slack member ids allowed to push messages into the session. `*` allows everyone (not recommended) |
| `SLACK_ALLOWED_BOT_IDS` | no | *(bots ignored)* | Bot ids (`B...` or their member `U...`) allowed to trigger the session |
| `SLACK_CHANNEL_IDS` | no | all joined channels | Restrict channel listening to these channel ids (does not affect DMs) |
| `SLACK_ALLOW_DMS` | no | `true` | Set `false` to ignore direct messages |
| `SLACK_REQUIRE_MENTION` | no | `true` | Set `false` to forward channel messages that don't @mention the bot |
| `SLACK_MENTION_WINDOW_SECONDS` | no | `60` | After a sender's message is forwarded, their follow-ups in the same channel pass the mention gate for this long (sliding). `0` disables |

With the mention requirement on, a channel message is treated as addressed to the bot if it @mentions the bot, is a **reply in a thread the bot has posted in**, or falls inside the sender's continuation window. Sender allowlists always apply regardless. Unlike the Discord plugin there is no role-mention concern — Slack mentions are always the literal `<@U...>` form — and another bot addressing yours must include that raw form in its message text.

## What the agent can do

- **`send_message`** — post to a channel or DM; pass `thread_ts` (from the incoming tag) to reply in-thread. Content is split at ~4,000 characters into consecutive messages.
- **`add_reaction`** — emoji-react to a message (`thumbsup`, `eyes`, `white_check_mark`, ...).
- **`read_messages`** — fetch recent channel history for context that wasn't forwarded.
- **`send_file`** — upload a file from the machine into a channel or DM (Slack's external-upload flow), with an optional caption. Needs the `files:write` scope.
- **`read_attachment`** — download an incoming Slack file (the `[attachment: url]` lines in forwarded messages) to a temp file and return its path, so the agent can read what you drop in the chat. Authenticates with the bot token (`files:read`), restricted to Slack's file hosts, 25 MB cap.

Delivery semantics are standard channels behavior: an idle session wakes immediately; a busy one receives queued events together at the end of the current turn.

## Slash commands from Slack

`/status` (alias `/session`), `/channels`, and `/help` are executed by the **host** and answered in the channel (threaded, if you sent the command from a thread) — the agent never sees them, and they work even mid-turn. One Slack quirk: the Slack client intercepts messages starting with `/` as its own slash commands, so type a leading space (`  /status`) to send the text literally. Unrecognized `/commands` and everything else forward to the agent as normal messages; bot-authored messages are never treated as commands.

## Troubleshooting

The bridge logs to stderr, captured at `$GROK_HOME/logs/mcp/slack.stderr.log` (truncated each session start):

- `connected to Slack as grok (U...)` then `socket mode ready (hello received)` → healthy. If messages still don't arrive, look for `dropping message ...: sender <name> (id U...) is not in SLACK_ALLOWED_USER_IDS` — that logged id is exactly what belongs in your allowlist.
- `auth.test failed: invalid_auth` → bad `SLACK_BOT_TOKEN`.
- `apps.connections.open failed` → bad `SLACK_APP_TOKEN` or Socket Mode not enabled for the app.
- Connected but nothing when messaging a channel → the bot isn't in that channel (`/invite @grok`), the message didn't mention it (and no window/thread applied), or the `message.channels` event subscription is missing.
- No new log lines at all since startup → no message has been sent since this session's bridge started.

## Security

Same model as the Discord plugin: sender allowlist is mandatory (gate on identity, not the room), bots are ignored unless explicitly allowlisted (mind mention loops — keep it one-directional unless both sides have loop guards), channel text is untrusted input in front of your agent, and tokens grant real workspace access — keep the `.env` at mode `600` and out of version control.

## Development

Single zero-dependency Node (≥ 22) script: [`server/slack-channel.mjs`](server/slack-channel.mjs) — MCP over stdio with the `grok/channel` capability, Socket Mode connection (fresh single-use URL per connect, envelope acks, server-requested refresh, backoff reconnect), and the same gate structure as the Discord bridge.

Run the end-to-end test (mock Socket Mode + mock Web API, no network or tokens needed):

```sh
node plugins/slack/server/slack-channel.test.mjs
```
