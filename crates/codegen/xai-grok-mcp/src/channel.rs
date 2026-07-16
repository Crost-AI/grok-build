//! Channel protocol types — server-pushed session events.
//!
//! A *channel* is an MCP server that pushes events into a running Grok
//! Build session (chat bridges, webhook receivers, CI alerts). The
//! protocol rides standard MCP with two extensions:
//!
//! 1. The server declares the experimental capability
//!    [`CHANNEL_CAPABILITY_KEY`] (`grok/channel`) in its `initialize`
//!    result. Presence is what marks the server as a channel; the value
//!    is an empty object reserved for future settings.
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

    #[test]
    fn underscore_and_digits_are_valid_key_chars() {
        assert!(is_valid_meta_key("chat_id"));
        assert!(is_valid_meta_key("run2"));
        assert!(is_valid_meta_key("_leading"));
        assert!(!is_valid_meta_key("dash-ed"));
        assert!(!is_valid_meta_key(""));
    }
}
