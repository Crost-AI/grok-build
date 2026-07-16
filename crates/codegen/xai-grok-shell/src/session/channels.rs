//! Session-side channels support: opt-in resolution, gating policy,
//! and the `<channel>` envelope.
//!
//! A *channel* is an MCP server that pushes events into the running
//! session (see `xai_grok_mcp::channel` for the wire protocol). This
//! module owns everything session-side:
//!
//! - [`ChannelSpec`] — parsed `--channels` entries
//!   (`server:<name>` / `plugin:<name>@<marketplace>`).
//! - [`ChannelsPolicy`] / [`resolve_channels_policy`] — the
//!   `[channels]` config gate (`enabled` master switch + optional
//!   `allowed` entry allowlist).
//! - [`ChannelRegistry`] — which MCP servers are opted in as channels
//!   for this session, plus per-entry state for `/channels`.
//! - [`format_channel_event`] — the `<channel source=... k=v>body`
//!   envelope injected into the conversation.
//! - [`load_channel_env_file`] — `~/.grok/channels/<server>/.env`
//!   credential loading, merged into the channel server's spawn env.
//!
//! Security model (mirrors the documented contract): being configured
//! as an MCP server is NOT enough to push messages — the server must
//! also be named in `--channels` for the session, pass the `[channels]
//! allowed` allowlist when one is set, and `[channels] enabled` must
//! not be false. Events from servers that fail any gate are dropped
//! silently. Sender-level gating (who may talk to the bot / POST to
//! the webhook) is the channel server's responsibility.

use std::collections::HashMap;

use toml::Value as TomlValue;
use xai_grok_mcp::channel::ChannelInboundEvent;

/// Content bodies larger than this are truncated before injection so a
/// misbehaving channel can't flood the context window in one event.
const MAX_CONTENT_CHARS: usize = 100_000;

/// Marker appended to a truncated event body.
const TRUNCATION_MARKER: &str = "\n[... truncated by grok: channel event exceeded size limit]";

/// One parsed `--channels` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelSpec {
    /// `server:<name>` — an MCP server from config.toml / `.mcp.json`,
    /// matched by exact server name.
    Server { name: String },
    /// `plugin:<name>@<marketplace>` — every MCP server contributed by
    /// the named plugin. The marketplace segment is kept for display
    /// and allowlist matching; plugin resolution matches on name.
    Plugin { name: String, marketplace: String },
}

impl ChannelSpec {
    /// Parse a `--channels` entry. Returns `Err` with a user-facing
    /// message for malformed entries so startup can surface it.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if let Some(name) = raw.strip_prefix("server:") {
            if name.is_empty() {
                return Err(format!(
                    "invalid --channels entry '{raw}': empty server name"
                ));
            }
            return Ok(Self::Server {
                name: name.to_string(),
            });
        }
        if let Some(rest) = raw.strip_prefix("plugin:") {
            let Some((name, marketplace)) = rest.split_once('@') else {
                return Err(format!(
                    "invalid --channels entry '{raw}': expected plugin:<name>@<marketplace>"
                ));
            };
            if name.is_empty() || marketplace.is_empty() {
                return Err(format!(
                    "invalid --channels entry '{raw}': expected plugin:<name>@<marketplace>"
                ));
            }
            return Ok(Self::Plugin {
                name: name.to_string(),
                marketplace: marketplace.to_string(),
            });
        }
        Err(format!(
            "invalid --channels entry '{raw}': expected server:<name> or plugin:<name>@<marketplace>"
        ))
    }

    /// Canonical spec string (`server:x` / `plugin:x@y`) — the form
    /// used in `[channels] allowed` and in `/channels` output.
    pub fn display(&self) -> String {
        match self {
            Self::Server { name } => format!("server:{name}"),
            Self::Plugin { name, marketplace } => format!("plugin:{name}@{marketplace}"),
        }
    }
}

/// Resolved `[channels]` config gate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelsPolicy {
    /// Master switch. `false` (from any layer) blocks all channel
    /// delivery for the session; `--channels` entries still show in
    /// `/channels` with a blocked state so the user learns why.
    pub enabled: bool,
    /// When `Some`, only entries whose canonical spec string appears
    /// here may register. `None` = any `--channels` entry is allowed
    /// (opting in per session is already an explicit user action).
    pub allowed: Option<Vec<String>>,
}

/// Read `[channels]` from one TOML layer.
fn channels_table(v: Option<&TomlValue>) -> Option<&TomlValue> {
    v?.get("channels")
}

/// Resolve the `[channels]` policy across config layers.
///
/// Precedence for `enabled` follows the repo-wide `BoolFlag` order:
/// `requirements.toml` (enterprise enforcement) > `GROK_CHANNELS_ENABLED`
/// env > user `config.toml` > default (`true`). For `allowed`, the
/// requirements layer wins over user config when both set a list;
/// there is no env form.
pub fn resolve_channels_policy(
    requirements: Option<&TomlValue>,
    user: Option<&TomlValue>,
) -> ChannelsPolicy {
    fn enabled_from(v: Option<&TomlValue>) -> Option<bool> {
        channels_table(v)?.get("enabled")?.as_bool()
    }
    fn allowed_from(v: Option<&TomlValue>) -> Option<Vec<String>> {
        Some(
            channels_table(v)?
                .get("allowed")?
                .as_array()?
                .iter()
                .filter_map(|e| e.as_str().map(String::from))
                .collect(),
        )
    }
    let env_enabled = std::env::var("GROK_CHANNELS_ENABLED")
        .ok()
        .and_then(|v| match v.trim() {
            "1" | "true" | "TRUE" | "True" => Some(true),
            "0" | "false" | "FALSE" | "False" => Some(false),
            _ => None,
        });
    let enabled = enabled_from(requirements)
        .or(env_enabled)
        .or(enabled_from(user))
        .unwrap_or(true);
    let allowed = allowed_from(requirements).or_else(|| allowed_from(user));
    ChannelsPolicy { enabled, allowed }
}

/// Why an opted-in entry is (or is not) live. Surfaced by `/channels`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelState {
    /// Gates passed; events from this server inject into the session.
    Active,
    /// `[channels] enabled = false` — nothing delivers this session.
    BlockedDisabled,
    /// `[channels] allowed` is set and does not include this entry.
    BlockedNotAllowed,
    /// The `--channels` entry didn't parse; carries the parse error.
    Invalid(String),
    /// The entry parsed but matched no configured MCP server (typo,
    /// plugin without MCP servers, or server not configured for this
    /// project).
    Unmatched,
}

/// One `--channels` entry with its resolution result.
#[derive(Debug, Clone)]
pub struct ChannelEntry {
    /// The entry as the user wrote it.
    pub raw: String,
    /// MCP server names this entry resolved to (empty when
    /// unmatched/invalid/blocked).
    pub servers: Vec<String>,
    pub state: ChannelState,
}

/// Per-session channel opt-in table, built once at session start by
/// `SessionActor::setup_channels` and read by the event forwarder and
/// `/channels`.
#[derive(Debug, Default)]
pub struct ChannelRegistry {
    /// Every `--channels` entry, in the order given.
    pub entries: Vec<ChannelEntry>,
    /// server name → canonical spec string, for the delivery gate.
    /// Only servers from `Active` entries appear here.
    pub active_servers: HashMap<String, String>,
}

impl ChannelRegistry {
    /// Delivery gate used by the inbound-event forwarder.
    pub fn is_active(&self, server: &str) -> bool {
        self.active_servers.contains_key(server)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Escape a string for use inside a double-quoted XML-ish attribute.
fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render an inbound channel event as the `<channel>` envelope
/// injected into the conversation:
///
/// ```text
/// <channel source="telegram" chat_id="123">
/// what's the status of the build?
/// </channel>
/// ```
///
/// The `source` attribute is always first and set from the MCP server
/// name; each sanitized meta entry (see
/// `xai_grok_mcp::channel::parse_channel_notification`) becomes one
/// attribute. The body is verbatim apart from the size cap.
pub fn format_channel_event(event: &ChannelInboundEvent) -> String {
    let mut tag = format!("<channel source=\"{}\"", escape_attr(&event.server));
    for (key, value) in &event.meta {
        // Keys are already identifier-only (parse-time sanitization);
        // values still need escaping.
        tag.push_str(&format!(" {key}=\"{}\"", escape_attr(value)));
    }
    tag.push('>');
    let mut content: &str = &event.content;
    let mut truncated = false;
    if content.chars().count() > MAX_CONTENT_CHARS {
        let byte_end = content
            .char_indices()
            .nth(MAX_CONTENT_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(content.len());
        content = &content[..byte_end];
        truncated = true;
    }
    let mut out = tag;
    out.push('\n');
    out.push_str(content);
    if truncated {
        out.push_str(TRUNCATION_MARKER);
    }
    out.push_str("\n</channel>");
    out
}

/// Directory holding per-channel credentials:
/// `~/.grok/channels/<server>/`.
pub fn channel_env_dir(server: &str) -> std::path::PathBuf {
    xai_grok_tools::util::grok_home::grok_home()
        .join("channels")
        .join(server)
}

/// Parse a `.env`-style file (`KEY=VALUE` lines, `#` comments, blank
/// lines, optional single/double quotes around the value). Returns an
/// empty list when the file is missing or unreadable — credentials are
/// optional.
pub fn load_channel_env_file(path: &std::path::Path) -> Vec<(String, String)> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut vars = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        vars.push((key.to_string(), value.to_string()));
    }
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(server: &str, content: &str, meta: &[(&str, &str)]) -> ChannelInboundEvent {
        ChannelInboundEvent {
            server: server.to_string(),
            content: content.to_string(),
            meta: meta
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    // ── ChannelSpec ────────────────────────────────────────────────

    #[test]
    fn parses_server_and_plugin_specs() {
        assert_eq!(
            ChannelSpec::parse("server:webhook"),
            Ok(ChannelSpec::Server {
                name: "webhook".to_string()
            })
        );
        assert_eq!(
            ChannelSpec::parse("plugin:telegram@grok-plugins-official"),
            Ok(ChannelSpec::Plugin {
                name: "telegram".to_string(),
                marketplace: "grok-plugins-official".to_string()
            })
        );
    }

    #[test]
    fn rejects_malformed_specs() {
        for raw in [
            "webhook",
            "server:",
            "plugin:telegram",
            "plugin:@market",
            "plugin:name@",
            "",
        ] {
            assert!(ChannelSpec::parse(raw).is_err(), "should reject {raw:?}");
        }
    }

    #[test]
    fn spec_display_round_trips() {
        for raw in ["server:webhook", "plugin:telegram@official"] {
            assert_eq!(ChannelSpec::parse(raw).unwrap().display(), raw);
        }
    }

    // ── Policy resolution ──────────────────────────────────────────

    fn toml(s: &str) -> TomlValue {
        // NB: `str::parse::<toml::Value>()` parses a single VALUE, not a
        // document — a `[section]` header would fail. Deserialize the
        // document the way the real config loader does.
        toml::from_str(s).unwrap()
    }

    #[test]
    fn policy_defaults_to_enabled_with_no_allowlist() {
        let p = resolve_channels_policy(None, None);
        assert!(p.enabled);
        assert!(p.allowed.is_none());
    }

    #[test]
    fn user_config_can_disable_and_restrict() {
        let user = toml("[channels]\nenabled = false\nallowed = [\"server:x\"]");
        let p = resolve_channels_policy(None, Some(&user));
        assert!(!p.enabled);
        assert_eq!(p.allowed, Some(vec!["server:x".to_string()]));
    }

    #[test]
    fn requirements_layer_wins_over_user_config() {
        let req = toml("[channels]\nenabled = false");
        let user = toml("[channels]\nenabled = true\nallowed = [\"server:y\"]");
        let p = resolve_channels_policy(Some(&req), Some(&user));
        assert!(!p.enabled);
        // requirements has no allowlist → user's applies.
        assert_eq!(p.allowed, Some(vec!["server:y".to_string()]));
    }

    #[test]
    fn empty_allowed_list_blocks_everything() {
        let user = toml("[channels]\nallowed = []");
        let p = resolve_channels_policy(None, Some(&user));
        assert!(p.enabled);
        assert_eq!(p.allowed, Some(Vec::new()));
    }

    // ── Envelope ───────────────────────────────────────────────────

    #[test]
    fn formats_basic_envelope() {
        let ev = event("webhook", "build failed on main", &[]);
        assert_eq!(
            format_channel_event(&ev),
            "<channel source=\"webhook\">\nbuild failed on main\n</channel>"
        );
    }

    #[test]
    fn meta_becomes_attributes_in_order() {
        let ev = event("telegram", "hi", &[("chat_id", "42"), ("sender", "karl")]);
        assert_eq!(
            format_channel_event(&ev),
            "<channel source=\"telegram\" chat_id=\"42\" sender=\"karl\">\nhi\n</channel>"
        );
    }

    #[test]
    fn attribute_values_are_escaped_but_body_is_verbatim() {
        let ev = event(
            "web\"hook",
            "a < b & c > d \"quoted\"",
            &[("path", "/x?a=1&b=\"<2>\"")],
        );
        let out = format_channel_event(&ev);
        assert!(out.starts_with(
            "<channel source=\"web&quot;hook\" path=\"/x?a=1&amp;b=&quot;&lt;2&gt;&quot;\">"
        ));
        // The body is not entity-escaped.
        assert!(out.contains("\na < b & c > d \"quoted\"\n"));
    }

    #[test]
    fn newlines_in_attribute_values_cannot_break_the_tag_line() {
        let ev = event("chan", "body", &[("k", "line1\nline2")]);
        let out = format_channel_event(&ev);
        let tag_line = out.lines().next().unwrap();
        assert!(tag_line.contains("k=\"line1&#10;line2\""));
    }

    #[test]
    fn oversized_content_is_truncated_with_marker() {
        let big = "x".repeat(MAX_CONTENT_CHARS + 10);
        let ev = event("chan", &big, &[]);
        let out = format_channel_event(&ev);
        assert!(out.contains(TRUNCATION_MARKER));
        assert!(out.len() < big.len() + 200);
    }

    // ── Registry gate ──────────────────────────────────────────────

    #[test]
    fn registry_gates_on_active_servers_only() {
        let mut reg = ChannelRegistry::default();
        reg.active_servers
            .insert("webhook".to_string(), "server:webhook".to_string());
        assert!(reg.is_active("webhook"));
        assert!(!reg.is_active("other"));
    }

    // ── .env loading ───────────────────────────────────────────────

    #[test]
    fn env_file_parsing_handles_comments_quotes_and_export() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "# comment\n\nTELEGRAM_BOT_TOKEN=123:abc\nexport QUOTED=\"a b\"\nSINGLE='c d'\nbad-key=zzz\nNOEQUALS\n",
        )
        .unwrap();
        let vars = load_channel_env_file(&path);
        assert_eq!(
            vars,
            vec![
                ("TELEGRAM_BOT_TOKEN".to_string(), "123:abc".to_string()),
                ("QUOTED".to_string(), "a b".to_string()),
                ("SINGLE".to_string(), "c d".to_string()),
            ]
        );
    }

    #[test]
    fn missing_env_file_is_empty() {
        assert!(load_channel_env_file(std::path::Path::new("/nonexistent/.env")).is_empty());
    }
}
