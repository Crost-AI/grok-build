//! Live MCP handshake against the bundled Discord channel bridge
//! (`plugins/discord/server/discord-channel.mjs`), exercising the same
//! client path a real session uses: spawn over stdio, initialize via
//! rmcp, then check the parsed `grok/channel` experimental capability.
//!
//! Requires `node` (>= 22) on PATH; skips (with a message) when absent
//! so CI images without Node don't fail. No token is configured — the
//! bridge serves MCP without a gateway connection in that mode.

use agent_client_protocol as acp;
use xai_grok_mcp::servers::{McpSpawnCtx, start_mcp_server};
use xai_grok_session_events::EventWriter;

fn bridge_path() -> std::path::PathBuf {
    // tests run with CWD = crate root (crates/codegen/xai-grok-mcp)
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../plugins/discord/server/discord-channel.mjs")
        .canonicalize()
        .expect("bundled discord bridge exists")
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_declares_channel_capability_through_real_handshake() {
    if !node_available() {
        eprintln!("skipping: node not on PATH");
        return;
    }
    let config = acp::McpServer::Stdio(
        acp::McpServerStdio::new("discord".to_string(), "node")
            .args(vec![bridge_path().to_string_lossy().into_owned()])
            .env(vec![]),
    );
    let event_writer = EventWriter::noop();
    let ctx = McpSpawnCtx::standalone(&event_writer);
    let client = start_mcp_server(config, None, None, None, &ctx)
        .await
        .expect("bridge spawns");

    let service = client
        .ensure_initialized()
        .await
        .expect("handshake succeeds");
    assert!(
        client.declares_channel_capability().await,
        "host must see the grok/channel experimental capability from the bridge's \
         initialize result; parsed peer info: {:?}",
        service.peer_info(),
    );

    let descriptor = client
        .channel_commands_descriptor()
        .await
        .expect("bridge declares the commands reply-routing descriptor");
    assert_eq!(descriptor.reply_tool, "send_message");
    assert_eq!(descriptor.target_meta, "channel_id");
    assert_eq!(descriptor.target_arg, "channel_id");
    assert_eq!(descriptor.content_arg, "content");
    // Native slash commands route host replies to the interaction
    // follow-up webhook via this meta pass-through.
    assert_eq!(
        descriptor.extra_args,
        vec![("interaction_token".to_string(), "interaction_token".to_string())]
    );

    let tools = service.list_tools(None).await.expect("tools/list succeeds");
    let mut names: Vec<_> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "add_reaction",
            "close_thread",
            "create_poll",
            "create_thread",
            "end_poll",
            "read_attachment",
            "read_messages",
            "read_poll",
            "reload_config",
            "rename_thread",
            "send_file",
            "send_message",
        ]
    );
}
