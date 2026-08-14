//! Channel protocol types — server-pushed session events.
//!
//! A *channel* is an MCP server that pushes events into a running Grok
//! Build session (chat bridges, webhook receivers, CI alerts). The
//! protocol rides standard MCP with two extensions:
//!
//! 1. The server declares the experimental capability
//!    [`CHANNEL_CAPABILITY_KEY`] (`grok/channel`) in its `initialize`
//!    result. Presence is what marks the server as a channel; the value
//!    is an object that may carry optional settings — today the
//!    `commands` reply-routing descriptor (see
//!    [`ChannelCommandsDescriptor`]), with unknown keys reserved.
//! 2. The server emits [`CHANNEL_NOTIFICATION_METHOD`]
//!    (`notifications/grok/channel`) notifications whose params carry
//!    the event:
//!
//!    ```json
//!    {
//!      "content": "build failed on main: https://ci.example.com/run/1234",
//!      "meta": { "severity": "high", "run_id": "1234" }
//!    }
//!    ```
//!
//! `content` is required; `meta` is an optional string→string map whose
//! entries become attributes on the `<channel>` tag rendered into the
//! conversation (see the shell's `session::channels` module for the
//! envelope). Meta keys must be identifiers (`[A-Za-z0-9_]+`); keys that
//! aren't — or that would collide with the auto-set `source` attribute —
//! are silently dropped, mirroring the documented contract so channel
//! authors get the same behavior across hosts.
//!
//! Delivery is fire-and-forget: notifications are unacknowledged and the
//! host drops events for servers that are not opted in as channels for
//! the session (`--channels` / `[channels]` config). Two-way channels
//! expose ordinary MCP tools for replies; nothing about the reply path
//! is channel-specific.

use std::sync::Arc;

use crate::servers::McpServerName;

/// Experimental-capability key a server sets to register as a channel.
pub const CHANNEL_CAPABILITY_KEY: &str = "grok/channel";

/// Notification method channels use to push an event into the session.
pub const CHANNEL_NOTIFICATION_METHOD: &str = "notifications/grok/channel";

/// One inbound channel event, parsed and sanitized from a
/// [`CHANNEL_NOTIFICATION_METHOD`] notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInboundEvent {
    /// Name of the MCP server that emitted the event. Becomes the
    /// `source` attribute of the rendered `<channel>` tag.
    pub server: McpServerName,
    /// Event body. Rendered verbatim as the `<channel>` tag body.
    pub content: String,
    /// Sanitized meta entries, in the order the server sent them
    /// (the workspace `serde_json` preserves object insertion order).
    /// Each entry becomes a tag attribute.
    pub meta: Vec<(String, String)>,
}

/// Reply-routing descriptor a channel server may declare under the
/// `"commands"` key of its [`CHANNEL_CAPABILITY_KEY`] capability value:
///
/// ```json
/// "grok/channel": {
///   "commands": {
///     "reply_tool": "send_message",
///     "target_meta": "channel_id",
///     "target_arg": "channel_id",
///     "content_arg": "content",
///     "extra_args": { "thread_ts": "thread_ts" }
///   }
/// }
/// ```
///
/// Declaring it opts the server into host-executed slash commands: when
/// an inbound event's body is a `/command` the host recognizes (and the
/// event is not bot-authored), the host runs the command itself and
/// routes the output back by calling `reply_tool` with
/// `{target_arg: <event meta[target_meta]>, content_arg: <output>}`,
/// instead of injecting the event into the model. `extra_args` names
/// additional tool arguments to copy from event meta when present
/// (argument name → meta key) — e.g. so replies land in the right
/// thread. Without the descriptor, command-looking events flow to the
/// model like any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelCommandsDescriptor {
    /// Tool to call with the command output (e.g. `send_message`).
    pub reply_tool: String,
    /// Event meta key holding the reply target (e.g. `channel_id`).
    /// Events missing this key cannot be replied to and are not
    /// intercepted.
    pub target_meta: String,
    /// Tool argument name to pass the target as.
    pub target_arg: String,
    /// Tool argument name to pass the command output as.
    pub content_arg: String,
    /// Additional `(tool argument name, event meta key)` pairs copied
    /// into the call when the meta key is present on the event.
    pub extra_args: Vec<(String, String)>,
}

/// Parse the `commands` descriptor out of a [`CHANNEL_CAPABILITY_KEY`]
/// capability *value*. Returns `None` when the value has no `commands`
/// object or any required field is missing, empty, or not a string —
/// a malformed descriptor disables host-side commands rather than
/// producing misrouted tool calls.
pub fn parse_channel_commands_descriptor(
    capability_value: &serde_json::Map<String, serde_json::Value>,
) -> Option<ChannelCommandsDescriptor> {
    let commands = capability_value.get("commands")?.as_object()?;
    let required = |key: &str| -> Option<String> {
        commands
            .get(key)?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut extra_args: Vec<(String, String)> = Vec::new();
    if let Some(map) = commands.get("extra_args").and_then(|v| v.as_object()) {
        for (arg, meta_key) in map {
            if let Some(meta_key) = meta_key.as_str().filter(|s| !s.is_empty())
                && !arg.is_empty()
            {
                extra_args.push((arg.clone(), meta_key.to_string()));
            }
        }
    }
    Some(ChannelCommandsDescriptor {
        reply_tool: required("reply_tool")?,
        target_meta: required("target_meta")?,
        target_arg: required("target_arg")?,
        content_arg: required("content_arg")?,
        extra_args,
    })
}

/// Shared sender slot for inbound channel events — same
/// ownership/wiring pattern as [`crate::servers::SharedEventTx`]: the
/// Arc lives on the `McpClient` and the `GrokClientHandler` it
/// constructs, so a sender installed post-handshake is observed by the
/// live rmcp service loop on the next notification.
pub type SharedChannelEventTx =
    Arc<parking_lot::Mutex<Option<tokio::sync::mpsc::UnboundedSender<ChannelInboundEvent>>>>;

/// Meta keys must be identifiers: ASCII letters, digits, underscores.
/// Anything else (hyphens, spaces, quotes, ...) is dropped rather than
/// escaped so a hostile payload can't smuggle attribute syntax into the
/// rendered tag.
fn is_valid_meta_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse a [`CHANNEL_NOTIFICATION_METHOD`] params object into a
/// [`ChannelInboundEvent`].
///
/// Returns `None` when `content` is missing or not a string — a
/// malformed notification is dropped, never surfaced to the model.
/// Invalid meta keys, non-string meta values, and the reserved key
/// `source` are dropped silently per the protocol contract.
pub fn parse_channel_notification(
    server: &str,
    params: Option<&serde_json::Value>,
) -> Option<ChannelInboundEvent> {
    let params = params?.as_object()?;
    let content = params.get("content")?.as_str()?.to_string();
    let mut meta: Vec<(String, String)> = Vec::new();
    if let Some(map) = params.get("meta").and_then(|m| m.as_object()) {
        for (key, value) in map {
            if key == "source" || !is_valid_meta_key(key) {
                continue;
            }
            if let Some(value) = value.as_str() {
                meta.push((key.clone(), value.to_string()));
            }
        }
    }
    Some(ChannelInboundEvent {
        server: server.to_string(),
        content,
        meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> Option<ChannelInboundEvent> {
        parse_channel_notification("chan", Some(&json))
    }

    #[test]
    fn parses_content_and_meta() {
        let ev = parse(serde_json::json!({
            "content": "build failed",
            "meta": { "severity": "high", "run_id": "1234" }
        }))
        .expect("valid event");
        assert_eq!(ev.server, "chan");
        assert_eq!(ev.content, "build failed");
        // The workspace serde_json preserves object insertion order.
        assert_eq!(
            ev.meta,
            vec![
                ("severity".to_string(), "high".to_string()),
                ("run_id".to_string(), "1234".to_string()),
            ]
        );
    }

    #[test]
    fn meta_is_optional() {
        let ev = parse(serde_json::json!({ "content": "ping" })).expect("valid event");
        assert!(ev.meta.is_empty());
    }

    #[test]
    fn missing_or_non_string_content_is_dropped() {
        assert!(parse(serde_json::json!({ "meta": {} })).is_none());
        assert!(parse(serde_json::json!({ "content": 42 })).is_none());
        assert!(parse_channel_notification("chan", None).is_none());
        assert!(parse_channel_notification("chan", Some(&serde_json::json!("str"))).is_none());
    }

    #[test]
    fn invalid_meta_keys_and_values_are_dropped_silently() {
        let ev = parse(serde_json::json!({
            "content": "x",
            "meta": {
                "ok_key1": "kept",
                "bad-key": "dropped (hyphen)",
                "bad key": "dropped (space)",
                "": "dropped (empty)",
                "k\"ey": "dropped (quote)",
                "num": 7,
                "source": "dropped (reserved — collides with the auto-set attribute)"
            }
        }))
        .expect("valid event");
        assert_eq!(ev.meta, vec![("ok_key1".to_string(), "kept".to_string())]);
    }

    fn capability_value(json: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        json.as_object().expect("test value is an object").clone()
    }

    #[test]
    fn parses_commands_descriptor() {
        let descriptor = parse_channel_commands_descriptor(&capability_value(serde_json::json!({
            "commands": {
                "reply_tool": "send_message",
                "target_meta": "channel_id",
                "target_arg": "channel_id",
                "content_arg": "content",
                "extra_args": { "thread_ts": "thread_ts", "": "dropped", "bad": "" }
            }
        })))
        .expect("valid descriptor");
        assert_eq!(descriptor.reply_tool, "send_message");
        assert_eq!(descriptor.target_meta, "channel_id");
        assert_eq!(descriptor.target_arg, "channel_id");
        assert_eq!(descriptor.content_arg, "content");
        assert_eq!(
            descriptor.extra_args,
            vec![("thread_ts".to_string(), "thread_ts".to_string())]
        );
    }

    #[test]
    fn commands_descriptor_optional_and_strict() {
        // Empty capability value (the pre-commands form) — no descriptor.
        assert!(
            parse_channel_commands_descriptor(&capability_value(serde_json::json!({}))).is_none()
        );
        // commands present but a required field missing or wrong type.
        assert!(
            parse_channel_commands_descriptor(&capability_value(serde_json::json!({
                "commands": { "reply_tool": "send_message" }
            })))
            .is_none()
        );
        assert!(
            parse_channel_commands_descriptor(&capability_value(serde_json::json!({
                "commands": {
                    "reply_tool": "send_message",
                    "target_meta": "channel_id",
                    "target_arg": 7,
                    "content_arg": "content"
                }
            })))
            .is_none()
        );
        // Empty string fields are rejected, extra_args stays optional.
        assert!(
            parse_channel_commands_descriptor(&capability_value(serde_json::json!({
                "commands": {
                    "reply_tool": "",
                    "target_meta": "channel_id",
                    "target_arg": "channel_id",
                    "content_arg": "content"
                }
            })))
            .is_none()
        );
        let minimal = parse_channel_commands_descriptor(&capability_value(serde_json::json!({
            "commands": {
                "reply_tool": "send_message",
                "target_meta": "channel_id",
                "target_arg": "channel_id",
                "content_arg": "content"
            }
        })))
        .expect("minimal descriptor");
        assert!(minimal.extra_args.is_empty());
    }

    #[test]
    fn underscore_and_digits_are_valid_key_chars() {
        assert!(is_valid_meta_key("chat_id"));
        assert!(is_valid_meta_key("run2"));
        assert!(is_valid_meta_key("_leading"));
        assert!(!is_valid_meta_key("dash-ed"));
        assert!(!is_valid_meta_key(""));
    }
}
