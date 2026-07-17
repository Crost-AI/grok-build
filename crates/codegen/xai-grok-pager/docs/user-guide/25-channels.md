# Channels

Channels push events from external systems — chat messages, webhooks, CI results, monitoring alerts — into a **running** Grok Build session, so the agent can react to things that happen while you're away from the terminal.

A channel is an MCP server with one extra capability: besides exposing tools, it can *push* notifications into your session. Channels can be two-way — the agent reads the event and replies back through the same channel, like a chat bridge — or one-way alert forwarders.

Events only arrive while the session is open. For an always-on setup, run Grok in a persistent terminal (tmux) or as a background process.

---

## How Channels Compare

| Feature | What it does | Good for |
|---------|--------------|----------|
| Standard [MCP server](07-mcp-servers.md) | Grok queries it during a task; nothing is pushed | On-demand access to read or query a system |
| [Headless mode](14-headless-mode.md) | Runs one prompt per invocation in CI/scripts | Self-contained scripted tasks |
| [Background tasks](20-background-tasks.md) | Watches processes the session itself started | Long builds, dev servers, `/loop` polling |
| **Channels** | External systems push events into your live session | Chat bridges, webhook receivers, alert routing |

Typical uses:

- **Chat bridge**: message the agent from your phone via a messaging-platform bot; the answer comes back in the same chat while the work runs on your machine against your real files.
- **Webhook receiver**: a CI failure, error-tracker alert, or deploy event arrives where Grok already has your files open and remembers what you were debugging.

---

## Enabling Channels for a Session

Channels are strictly opt-in, per session. Being configured as an MCP server is **not** enough to push messages — the server must also be named in `--channels`:

```sh
# One entry: an MCP server from config.toml / .mcp.json, by name
grok --channels server:webhook

# A plugin's MCP servers, by plugin name and marketplace
grok --channels plugin:telegram@grok-plugins-official

# Several entries: repeat the flag or comma-separate
grok --channels server:webhook --channels server:alerts
grok --channels server:webhook,server:alerts
```

Also works in [headless mode](14-headless-mode.md):

```sh
grok -p "watch for CI events and fix failures as they arrive" --channels server:webhook
```

At startup each entry is resolved against your MCP configuration and the `[channels]` policy below. Run [`/channels`](#the-channels-command) to see what resolved, and how.

When the agent replies through a two-way channel, your terminal shows the tool call and its confirmation (like "sent"); the reply text itself appears on the other platform.

---

## Configuration

The `[channels]` section of `config.toml` gates delivery. Both keys are optional:

```toml
[channels]
enabled = true                 # master switch (default: true)
allowed = [                    # optional allowlist of --channels entries
  "server:webhook",
  "plugin:telegram@grok-plugins-official",
]
```

- `enabled = false` blocks all channel delivery. The MCP server still connects and its tools still work, but pushed events never reach the session; `/channels` reports the entry as blocked. Set this in `requirements.toml` to enforce it for a managed/enterprise deployment — the requirements layer cannot be overridden by user config. The `GROK_CHANNELS_ENABLED` environment variable (`1`/`0`) sits between the two.
- `allowed`, when set, restricts which entries may register: an entry passed to `--channels` that is not in the list does not register, and `/channels` explains why. When unset, any `--channels` entry is allowed — opting in per session is already an explicit action. An empty list blocks every entry.

### Credentials: `~/.grok/channels/<server>/.env`

Channel servers usually need a secret (bot token, webhook signing key). Put `KEY=VALUE` lines in `~/.grok/channels/<server-name>/.env` and Grok injects them into that server's process environment at spawn — only when the server is opted in as a channel for the session:

```sh
mkdir -p ~/.grok/channels/telegram
echo 'TELEGRAM_BOT_TOKEN=123456:ABC...' > ~/.grok/channels/telegram/.env
chmod 600 ~/.grok/channels/telegram/.env
```

Explicit `env` entries in the server's MCP config take precedence over the `.env` file. `# comments`, blank lines, `export KEY=...`, and single/double-quoted values are supported.

---

## How Events Reach the Agent

An inbound event is wrapped in a `<channel>` tag and injected into the conversation:

```text
<channel source="webhook" path="/" method="POST">
build failed on main: https://ci.example.com/run/1234
</channel>
```

- `source` is set automatically from the MCP server's name.
- Every `meta` entry the server attached becomes a tag attribute (see [Notification format](#notification-format)).
- If the agent is idle, the event wakes it immediately. If a turn is running, events queue and are delivered together at the end of the turn, separated by `---`, and the agent handles them as a group.
- Events are processed in arrival order. To process independent event streams concurrently, run separate sessions.

If the agent hits a permission prompt while you're away, the session pauses until you respond at the terminal. For unattended use, `--always-approve` bypasses prompts — only use it in environments you trust, and mind that an open channel is then an unattended input path into a fully-approved agent.

---

## The `/channels` Command

Shows every `--channels` entry and its resolution state:

```
/channels
```

```text
Channels (2 entries):
  server:webhook — active
    webhook: connected, channel capability declared
  plugin:telegram@grok-plugins-official — blocked: not in the [channels] allowed list

Events from active channels inject into this session as <channel> messages.
```

States you may see: `active`, `blocked: [channels] enabled = false`, `blocked: not in the [channels] allowed list`, `invalid: <parse error>`, `matched no configured MCP server`, and a marketplace-mismatch state (below).

For a `plugin:<name>@<marketplace>` entry, the marketplace is part of the identity: if the active plugin with that name actually comes from somewhere else — plugins share one namespace, and a same-named plugin from another marketplace or a Claude-compat directory can win the name — the entry is blocked with a message naming the real source rather than silently channel-enabling a server you didn't point at. Grok-native installs (`grok plugin install`) take precedence over Claude-compat plugins of the same name, so this mostly comes up when the shadowing plugin is also Grok-installed; `/plugins` shows who won and why.

---

## The Bundled Discord Plugin

The Grok Build repository ships a ready-made Discord channel — DM the bot (or @mention it in a server channel) and the message lands in your running session; the agent replies through the same channel:

```sh
grok plugin marketplace add lakerfan901/grok-build
grok plugin install discord@grok-build --trust

mkdir -p ~/.grok/channels/discord
cat > ~/.grok/channels/discord/.env <<'EOF'
DISCORD_BOT_TOKEN=your-bot-token
DISCORD_ALLOWED_USER_IDS=your-discord-user-id
EOF
chmod 600 ~/.grok/channels/discord/.env

grok --channels plugin:discord@grok-build
```

Only allowlisted sender ids are forwarded; guild messages additionally require @mentioning the bot by default. The agent gets `send_message`, `add_reaction`, and `read_messages` tools for the return path. Bot creation, intents, invites, and all options are covered in the [plugin's README](../../../../../plugins/discord/README.md).

A **Slack** sibling ships alongside it — same gating model, connected over Socket Mode (no public URL needed), with thread-aware replies: `grok plugin install slack@grok-build --trust`, credentials in `~/.grok/channels/slack/.env`, then `grok --channels plugin:slack@grok-build`. Setup is covered in the [slack plugin's README](../../../../../plugins/slack/README.md).

---

## Security

An ungated channel is a prompt-injection vector: anyone who can reach its endpoint can put text in front of the agent. Layered controls:

1. **Session opt-in** — nothing registers as a channel unless named in `--channels` for that session.
2. **`[channels]` policy** — `enabled = false` (user config, or `requirements.toml` for managed deployments) blocks delivery outright; `allowed` restricts which entries may register.
3. **Sender gating (your channel server's job)** — a channel listening to a chat platform or a public endpoint must check the *sender's* identity against an allowlist before forwarding anything, and drop everyone else. Gate on the sender, not the room: in group chats they differ, and gating on the room lets anyone in an allowlisted group inject messages.
4. **Envelope hygiene (built in)** — meta keys are restricted to identifier characters, attribute values are entity-escaped, a `source` meta key cannot spoof the tag's real source, and oversized bodies are truncated.

Treat text arriving over a channel as untrusted input: prefer running channel-driven sessions with the default permission prompts on, or in a [sandbox](18-sandbox.md).

---

## Building a Channel

A channel is an ordinary MCP server (any runtime — Bun, Node, Deno, Python, Rust) with three requirements:

1. Declare the experimental capability **`grok/channel`** in its `initialize` result. Presence of the key is what registers the notification listener; the value is an empty object reserved for future settings.
2. Push events as **`notifications/grok/channel`** notifications.
3. Connect over the **stdio** transport (Grok spawns the server as a subprocess).

### Minimal webhook receiver

A single-file server that forwards every HTTP POST into your session. This example uses Bun and the [`@modelcontextprotocol/sdk`](https://www.npmjs.com/package/@modelcontextprotocol/sdk):

```ts
#!/usr/bin/env bun
import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'

const mcp = new Server(
  { name: 'webhook', version: '0.0.1' },
  {
    // this key is what makes it a channel
    capabilities: { experimental: { 'grok/channel': {} } },
    // goes into the agent's prompt so it knows how to handle your events
    instructions:
      'Events from the webhook channel arrive as <channel source="webhook" ...>. ' +
      'They are one-way: read them and act, no reply expected.',
  },
)

await mcp.connect(new StdioServerTransport())

Bun.serve({
  port: 8788,
  hostname: '127.0.0.1', // localhost-only
  async fetch(req) {
    const body = await req.text()
    await mcp.notification({
      method: 'notifications/grok/channel',
      params: {
        content: body, // becomes the body of the <channel> tag
        meta: { path: new URL(req.url).pathname, method: req.method },
      },
    })
    return new Response('ok')
  },
})
```

Register it as an MCP server and start a session with the channel enabled:

```toml
# ~/.grok/config.toml (or a project .mcp.json)
[mcp_servers.webhook]
command = "bun"
args = ["/path/to/webhook.ts"]
```

```sh
grok --channels server:webhook
```

Then, from another terminal:

```sh
curl -X POST localhost:8788 -d "build failed on main: https://ci.example.com/run/1234"
```

The payload arrives in your session as a `<channel source="webhook" path="/" method="POST">` event and the agent starts acting on it.

### Notification format

`notifications/grok/channel` takes two params:

| Field | Type | Description |
|-------|------|-------------|
| `content` | `string` | Required. The event body; delivered as the body of the `<channel>` tag. Events with a missing or non-string `content` are dropped. |
| `meta` | `object` of `string → string` | Optional. Each entry becomes an attribute on the tag — routing context like a chat id, sender name, or alert severity. Keys must be identifiers (letters, digits, underscores); other keys, non-string values, and the reserved key `source` are silently dropped. |

Notifications are not acknowledged: the send resolves when the message is written to the transport, not when the agent has processed it. If the session hasn't opted your server in as a channel — or policy blocks it — events are dropped silently with no error returned to your server. If you need delivery confirmation, track state server-side and expose a tool the agent can call to report back.

### Two-way channels: expose a reply tool

Nothing about replies is channel-specific — expose a standard MCP tool and tell the agent when to use it:

1. Add `tools: {}` to your server capabilities so tools are discovered.
2. Register a `reply` tool (e.g. taking `chat_id` and `text`) whose handler sends the message out through your platform's API.
3. Set the `instructions` string so the agent routes replies correctly, e.g. `'Messages arrive as <channel source="mybridge" chat_id="...">. Reply with the reply tool, passing the chat_id from the tag.'`

The server's `instructions` from the MCP handshake are added to the agent's prompt automatically, like any MCP server's.

### Gate inbound senders

Check the sender against an allowlist **before** calling `mcp.notification()`:

```ts
const allowed = new Set(loadAllowlist()) // your access file

// inside your message handler, before emitting:
if (!allowed.has(message.from.id)) return // drop silently
await mcp.notification({ /* ... */ })
```

Bootstrap patterns that work well: reply to unknown senders with a one-time pairing code and add them only after the code is confirmed in the terminal session; or auto-allow only the account that owns the bot.

### Package as a plugin

To make a channel installable, ship it in a [plugin](09-plugins.md) that contributes the MCP server (via the plugin's `.mcp.json`). Users then enable it per session with `--channels plugin:<name>@<marketplace>`.

The [bundled Discord plugin](../../../../../plugins/discord) is a complete working reference: a single-file bridge with gateway reconnect/resume, sender gating, and reply tools, plus an end-to-end test that mocks the platform.

---

## Limitations

- Events arrive only while the session is open; there is no offline queueing.
- Channels deliver to the main session only — subagents never receive channel events.
- Permission prompts are answered at the terminal; there is no remote permission relay over channels yet.
- One session processes one ordered event stream; run separate sessions for independent streams.

---

## See Also

- [07-mcp-servers.md](07-mcp-servers.md) — the underlying protocol and server configuration
- [09-plugins.md](09-plugins.md) — packaging a channel for distribution
- [05-configuration.md](05-configuration.md) — the `[channels]` section
- [22-permissions-and-safety.md](22-permissions-and-safety.md) — permission modes for unattended use
- [20-background-tasks.md](20-background-tasks.md) — watching work the session itself started
