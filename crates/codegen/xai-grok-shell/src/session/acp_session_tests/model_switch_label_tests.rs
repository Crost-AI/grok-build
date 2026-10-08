use xai_grok_sampling_types::conversation::ConversationItem;

use super::support::{actor_with_mcp, build_actor, running_task_stub, stdio};
use crate::session::SwitchContextWindow;
use crate::session::compaction_config::AsyncCompactionCache;

const SWITCH_TARGET_LABEL: &str = "Aurora";

fn head_text(conv: &[ConversationItem]) -> String {
    let Some(ConversationItem::System(sys)) = conv.first() else {
        panic!(
            "conversation must start with a System item, got {:?}",
            conv.first()
        );
    };
    sys.content.as_ref().to_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_relabels_live_agent_and_system_head() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("spawn-time prompt"),
                ConversationItem::user("hi"),
            ]);

            actor
                .handle_set_session_model(crate::session::SessionModelSwitch {
                    sampling_config: xai_grok_sampler::SamplerConfig {
                        model: "switch-target".to_owned(),
                        context_window: 256_000,
                        ..xai_grok_sampler::SamplerConfig::default()
                    },
                    use_concise: false,
                    is_family_switch: false,
                    apply_prompt_override: true,
                    skip_prompt_rewrite: false,
                    auto_compact_threshold_percent: 85,
                    system_prompt_label: SWITCH_TARGET_LABEL.to_owned(),
                    context_window_selection: SwitchContextWindow::Set(None),
                    supported_context_windows: Vec::new(),
                })
                .await
                .expect("model switch succeeds");

            let conv = actor.chat_state_handle.get_conversation().await;
            let head = head_text(&conv);
            assert!(
                head.contains(SWITCH_TARGET_LABEL),
                "system head must render the new model's label, got: {head:.120}"
            );
            let agent = actor.agent.borrow();
            assert_eq!(
                SWITCH_TARGET_LABEL,
                agent.prompt_context().system_prompt_label
            );
            assert_eq!(agent.system_prompt(), head);
            assert_eq!(2, conv.len(), "the switch swaps only the head");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn relabel_skipped_mid_turn_keeps_prefire_cache() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            actor.compaction.prefire.store(AsyncCompactionCache {
                note1: "NOTE1".to_owned(),
                prefix_len: 1,
                fingerprint: 42,
                model_slug: "switch-target".to_owned(),
                pass1_latency_ms: 5,
            });
            actor.state.lock().await.running_task = Some(running_task_stub("running"));

            actor
                .relabel_agent_system_prompt(SWITCH_TARGET_LABEL.to_owned())
                .await;

            assert!(
                actor.compaction.prefire.has_cache(),
                "a skipped relabel must not drop a prefire built on the still-live prompt"
            );
            assert_eq!(
                xai_grok_agent::DEFAULT_SYSTEM_PROMPT_LABEL,
                actor.agent.borrow().prompt_context().system_prompt_label
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn zero_turn_rebuild_renders_switch_target_label() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;

            actor
                .handle_rebuild_agent_for_definition(
                    xai_grok_agent::AgentDefinition::default_grok_build(),
                    SWITCH_TARGET_LABEL.to_owned(),
                )
                .await
                .expect("zero-turn rebuild succeeds");

            let conv = actor.chat_state_handle.get_conversation().await;
            let head = head_text(&conv);
            assert!(
                head.contains(SWITCH_TARGET_LABEL),
                "rebuilt head must render the new model's label, got: {head:.120}"
            );
            let agent = actor.agent.borrow();
            assert_eq!(
                SWITCH_TARGET_LABEL,
                agent.prompt_context().system_prompt_label
            );
            assert_eq!(agent.system_prompt(), head);
        })
        .await;
}

/// Crost channels: a zero-turn rebuild rematerializes MCP servers from the raw config sources,
/// which drops the per-channel `.env` overlay; the rebuild must re-apply it so the diff keeps
/// the live channel server instead of respawning it without credentials.
#[tokio::test(flavor = "current_thread")]
async fn zero_turn_rebuild_keeps_channel_env_overlay() {
    tokio::task::LocalSet::new()
        .run_until(async {
            const CHANNEL: &str = "crost-rebuild-test-channel";
            let raw = stdio(CHANNEL, "/nonexistent/crost-rebuild-test-channel");
            let active_servers = std::collections::HashMap::from([(
                CHANNEL.to_owned(),
                format!("server:{CHANNEL}"),
            )]);
            let mut live = vec![raw.clone()];
            crate::session::channels::apply_channel_env_overlay(&mut live, &active_servers);
            let actor = actor_with_mcp(live.clone(), true, Vec::new()).await;
            actor
                .channel_registry
                .lock()
                .expect("channel_registry mutex poisoned")
                .active_servers = active_servers;
            *actor.initial_client_mcp_servers.borrow_mut() = vec![raw];
            let mut definition = xai_grok_agent::AgentDefinition::default_grok_build();
            definition.mcp_servers = vec![xai_grok_agent::config::McpServerRef::Inline {
                name: "seat-adder".to_owned(),
                config: serde_json::json!({
                    "type": "stdio",
                    "command": "/nonexistent/seat-adder",
                    "args": [],
                }),
            }];

            actor
                .handle_rebuild_agent_for_definition(definition, SWITCH_TARGET_LABEL.to_owned())
                .await
                .expect("zero-turn rebuild succeeds");

            let configs = actor.mcp_state.lock().await.configs.clone();
            let channel = configs
                .iter()
                .find(|c| crate::session::mcp_servers::mcp_server_name(c) == CHANNEL)
                .expect("channel server survives the rebuild");
            assert_eq!(
                &live[0], channel,
                "rebuilt channel server must carry the same channel env overlay as the live one"
            );
        })
        .await;
}
