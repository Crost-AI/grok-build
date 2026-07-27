# Discord channel bridge

**One Discord bridge, three CLIs.** This repo is the single source of truth for the Discord channel server used by:

- **Grok Build** — vendored at `plugins/discord/` in [lakerfan901/grok-build](https://github.com/lakerfan901/grok-build) (installable grok plugin)
- **Codex** — vendored at `codex-rs/channels/examples/discord/` in [lakerfan901/codex](https://github.com/lakerfan901/codex)
- **Claude Code** — installable directly from this repo as a plugin marketplace

The server ([`server/discord-channel.mjs`](server/discord-channel.mjs)) is a single-file, zero-dependency MCP stdio server for Node ≥ 22. It declares the channel capability for every host protocol it serves (`grok/channel`, `codex/channel`, `claude/channel`) and emits each inbound event once per namespace — hosts ignore methods they don't recognize, so the same literal file works under any of the three CLIs with no per-host configuration.

```text
you, on Discord:            @bot why is CI red on main?
your running session:       <channel source="discord" channel_id="..." author="karl" ...>
                            why is CI red on main?
                            </channel>
agent → send_message tool:  Two tests in auth_flow are failing after #482 — bisecting now.
```

## What the agent gets

- **`send_message`** — reply (auto-splits past Discord's 2000-char limit; threads via `reply_to_message_id`)
- **`add_reaction`** — cheap acknowledgement
- **`read_messages`** — fetch recent history
- **`create_poll` / `read_poll` / `end_poll`** — native Discord polls (bots can't cast native votes; the agent states its vote in chat and `read_poll` tallies humans)
- **`create_thread` / `rename_thread` / `close_thread`** — workstream threads (rename/close verify the target is a thread and can never touch a regular channel)
- **`send_file`** — upload a local file (10 MB bot limit; needs the **Attach Files** permission)
- **`reload_config`** — re-read the credentials file and apply it live (see [Reloading configuration](#reloading-configuration))
- **`read_attachment`** — download an incoming attachment to a temp file (Discord CDN hosts only, 25 MB cap; trailing punctuation is trimmed from copied URLs and expired signed links are re-signed via `/attachments/refresh-urls` and retried)

Every tool carries honest MCP annotations (readers read-only, the rest non-destructive closed-world), so annotation-aware hosts auto-approve them.

## Slash commands

- **Native Discord commands** — the bridge registers `/status`, `/channels`, `/help`, and `/ask <prompt>` as real application commands in every guild it joins (requires the `applications.commands` OAuth2 scope on the invite). `/status` and `/channels` are answered by the **host** (the interaction defers, then the answer arrives as the command's reply); `/ask` messages the agent without an @mention.
- **Plain text** — a message that *starts* with `/status`, `/session`, `/channels`, or `/help` (after any @mention) is intercepted and answered by the host; unknown `/commands` flow to the agent unchanged.

Only allowlisted humans can invoke either form; bot-authored messages are never treated as commands.

## Sender attribution

Every non-command message body begins with a bridge-generated attribution line, so the sender travels inside the text the model reads — not only in tag attributes:

```
<channel source="discord" channel_id="…" author="karl" author_id="…" sent_at="2026-07-27T14:22:00Z">
[from: karl (human) · 14:22 UTC]
Ariel thinks we should change this.
</channel>
```

Names inside the message ("Ariel") are thereby always subordinate to the `[from:]` line — the agent is instructed that only that line and the `author` attribute identify the sender. Peer agents are labeled `(bot: codex)`. A user who tries to open their message with a fake `[from:]` line gets it neutralized (`⟦from:` — only the bridge can author that line), and `sent_at` gives explicit ordering when messages arrive in quick succession.

## Multi-agent turn-taking

With `DISCORD_PEER_BOTS` set (the *other* agents' bots, `name:id` pairs), each bridge enforces a floor protocol on its own agent:

- **@-mention claims the floor.** When a human @-mentions exactly one peer agent, this agent's `send_message`/`create_poll` calls into that channel are refused until the mentioned agent answers (or the claim times out, default 10 min). Reading and reactions stay available.
- **Open floor.** A human message that names no specific agent arms the channel: the *first* agent to answer takes the hold, and everyone else is treated as if that agent had been @-mentioned.
- **React before commenting.** The floor-holder's answer is forwarded with `claim_response="true"`. Other agents must react on it before speaking: **👍** full agreement — and no message may follow; **✋** partial agreement/disagreement *or* additional context — unlocks a message; **👎** disagreement — unlocks a message. The response window closes when a human speaks again (default 5 min).
- Peer bots are implicitly allowlisted so their responses always reach the session, and claim responses bypass the mention requirement.

Each bridge gates only its own agent; the combination across bridges yields the fleet-wide protocol. Claims expire automatically so an offline agent can never deadlock a channel.

## Reloading configuration

Edit the `.env` and the running session picks it up — no exit, no resume. The bridge watches the file (mtime poll, 5s) and re-applies allowlists, channel scoping, DM/mention behavior, the permission channel, and the bot token; a changed token reconnects the gateway. You can also ask the agent to reload on demand, which calls the `reload_config` tool:

> reload your discord config

Two knobs: `DISCORD_CONFIG_WATCH=false` disables the watcher (leaving the tool), and `DISCORD_CONFIG_WATCH_SECONDS` changes the poll interval.

What a reload does **not** cover: changes to the bridge's own code (for example after `grok plugin update discord` or a `git pull` of this repo). The running process still has the old script loaded, so that needs a session restart.

## Claude Code tool-approval relay

Under Claude Code the bridge also declares `claude/channel/permission`: when the session hits a permission prompt, the bridge posts it to Discord (to `DISCORD_PERMISSION_CHANNEL_ID`, or wherever the session was last messaged from) and an allowlisted human replies `yes <id>` or `no <id>` to allow/deny from their phone. Approval replies are consumed, never forwarded as conversation. Grok Build and Codex never send these prompts, so the capability is inert for them.

## Setup

### 1. Create a Discord bot

1. [Discord developer portal](https://discord.com/developers/applications) → **New Application** → **Bot** → **Reset Token** (this is `DISCORD_BOT_TOKEN`).
2. **Bot → Privileged Gateway Intents**: enable **Message Content Intent**.
3. Invite it: **OAuth2 → URL Generator**, scopes `bot` **and** `applications.commands`, permissions **View Channels**, **Send Messages**, **Read Message History**, **Add Reactions**, **Attach Files**, **Create Public Threads** (optionally **Manage Threads** to rename/close threads the bot didn't create, and to lock threads).

Use a **separate bot per CLI** if you run more than one — each bridge process is one gateway identity.

### 2. Credentials

One `.env` file per CLI, mode `600`:

| CLI | Path |
|-----|------|
| Grok Build | `~/.grok/channels/discord/.env` |
| Codex | `~/.codex/channels/discord/.env` |
| Claude Code | `~/.claude/channels/discord/.env` |

```sh
DISCORD_BOT_TOKEN=your-bot-token
DISCORD_ALLOWED_USER_IDS=your-discord-user-id
```

Grok and Codex inject the file into the server's environment when the session opts the channel in. Claude Code doesn't inject — the bridge detects a Claude launch (via `CLAUDE_PLUGIN_ROOT`) and loads `~/.claude/channels/discord/.env` itself.

### 3. Enable per CLI

**Grok Build** (vendored plugin):

```sh
grok plugin marketplace add lakerfan901/grok-build
grok plugin install discord@grok-build --trust
grok --channels plugin:discord@grok-build
```

**Codex** (vendored example) — `~/.codex/config.toml`:

```toml
[mcp_servers.discord]
command = "node"
args = ["/path/to/codex/codex-rs/channels/examples/discord/server/discord-channel.mjs"]

[channels]
entries = ["server:discord"]   # or pass --channels server:discord per session
```

**Claude Code** (this repo is the marketplace):

```sh
/plugin marketplace add lakerfan901/discord-channel-bridge
/plugin install discord-bridge@discord-channel-bridge
claude --channels plugin:discord-bridge@discord-channel-bridge
```

Note for Claude Code on Team/Enterprise orgs: channels are gated by the org-level `channelsEnabled` setting, and a custom plugin may need to be added to `allowedChannelPlugins` (otherwise Anthropic's curated allowlist applies). For local testing there is `--dangerously-load-development-channels`.

## Configuration reference

All via environment (put them in the `.env` file):

| Variable | Required | Default | Meaning |
|----------|----------|---------|---------|
| `DISCORD_BOT_TOKEN` | yes | — | Bot token |
| `DISCORD_ALLOWED_USER_IDS` | to receive | *(everything dropped)* | Comma-separated user ids allowed to reach the session; `*` allows everyone (not recommended) |
| `DISCORD_ALLOWED_BOT_IDS` | no | *(bots ignored)* | Bot user ids allowed through the bot filter (mind mention loops) |
| `DISCORD_CHANNEL_IDS` | no | all visible | Guild channels to listen in; threads inherit their parent's access |
| `DISCORD_ALLOW_DMS` | no | `true` | `false` ignores DMs |
| `DISCORD_REQUIRE_MENTION` | no | `true` | `false` forwards unmentioned guild messages, marked `addressed="other"/"none"` for judgment-based listening |
| `DISCORD_MENTION_WINDOW_SECONDS` | no | `60` | Sliding continuation window after a forwarded message |
| `DISCORD_PERMISSION_CHANNEL_ID` | no | last-messaged channel | Where Claude tool-approval prompts are posted |
| `CHANNEL_NAMESPACES` | no | `grok,codex,claude` | Host protocols to serve |
| `CHANNEL_ENV_FILE` | no | auto-detected | Credentials file to load and to watch for live reloads |
| `DISCORD_CONFIG_WATCH` | no | `true` | `false` disables the automatic reload watcher |
| `DISCORD_CONFIG_WATCH_SECONDS` | no | `5` | Credentials-file poll interval |
| `DISCORD_PEER_BOTS` | no | — | Other agents' bots (`codex:123,claude:456`) — enables turn-taking |
| `DISCORD_CLAIM_TIMEOUT_SECONDS` | no | `600` | How long an unanswered @-claim holds the floor |
| `DISCORD_RESPONSE_WINDOW_SECONDS` | no | `300` | React-before-commenting window after a claim response |
| `DISCORD_ATTACHMENT_HOSTS`, `DISCORD_API_BASE`, `DISCORD_GATEWAY_URL` | no | Discord | Test overrides |

## Security model

- **Sender identity, not the room, is the gate.** Nothing is forwarded until `DISCORD_ALLOWED_USER_IDS` is set; drops are logged with the sender id you'd need to allowlist.
- Bots are ignored unless explicitly allowlisted (`bot="true"` meta when they are); bot-authored messages can never trigger host slash commands or permission approvals.
- Everything arriving over the channel is untrusted input in front of your agent; the instructions tell it so.
- `read_attachment` is host-allowlisted to Discord's CDN; the bot token stays in a mode-600 file.

## Development

```sh
node server/discord-channel.test.mjs
```

The e2e spawns the real bridge against a mock Discord gateway (RFC 6455) and mock REST API — no network, no token. It covers all three namespaces, gating, threads, polls, files, native slash commands, and the Claude permission relay.

### Vendoring

Grok Build and Codex consume this repo via `git subtree`; to pull an update into a fork:

```sh
# grok-build
git subtree pull --prefix=plugins/discord https://github.com/lakerfan901/discord-channel-bridge.git main --squash
# codex
git subtree pull --prefix=codex-rs/channels/examples/discord https://github.com/lakerfan901/discord-channel-bridge.git main --squash
```

Land changes here first; never edit the vendored copies directly.
