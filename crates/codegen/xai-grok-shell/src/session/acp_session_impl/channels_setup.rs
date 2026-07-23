//! Channel opt-in resolution and inbound-event forwarding for
//! `SessionActor`.
//!
//! `setup_channels` runs once during `run_session` startup, BEFORE the
//! MCP init task is spawned, so the credential env merge lands in
//! `McpState::configs` before any channel server process is launched.
//! `spawn_channel_forwarder` then owns the receiving end of the
//! channel-event pipe wired into `McpState` and turns each event into
//! an idle-gated pending notification (the same queue background-task
//! completions ride), so delivery semantics — wake when idle, batch
//! with `---` separators when busy — are shared, not reimplemented.

use super::*;
use crate::session::channels::{
    ChannelEntry, ChannelRegistry, ChannelSpec, ChannelState, channel_command_help,
    channel_env_dir, describe_plugin_origin, format_channel_event, load_channel_env_file,
    parse_channel_command, plugin_origin_marketplace, resolve_channels_policy,
};

impl SessionActor {
    /// Resolve the session's `--channels` entries against the
    /// `[channels]` policy and the configured MCP server list, merge
    /// per-channel `.env` credentials into stdio spawn configs, and
    /// publish the result to `self.channel_registry`.
    ///
    /// Returns `true` when at least one entry resolved to an active
    /// channel server (i.e. the caller should wire the event pipe and
    /// spawn the forwarder).
    pub(super) async fn setup_channels(&self) -> bool {
        let raw_entries = self.startup_hints.channels.clone();
        if raw_entries.is_empty() {
            return false;
        }
        let user_cfg = crate::config::load_effective_config().ok();
        let requirements = crate::agent::config::read_requirements_toml();
        let policy = resolve_channels_policy(requirements.as_ref(), user_cfg.as_ref());

        // Server name → owning plugin name, for `plugin:` specs. Reuses
        // the sourced merge so plugin attribution matches what the MCP
        // config loader actually did.
        let plugin_registry = self.plugin_registry.borrow().clone();
        let plugin_servers: Vec<(String, String)> = {
            match &plugin_registry {
                Some(reg) => crate::session::managed_mcp::merge_managed_mcp_servers_sourced(
                    std::path::Path::new(&self.session_info.cwd),
                    Some(reg),
                    &self.rebuild_spec.compat,
                )
                .into_iter()
                .filter_map(|(server, source)| match source {
                    xai_grok_tools::types::config_source::ConfigSource::Plugin {
                        plugin_name,
                        ..
                    } => Some((
                        xai_grok_mcp::servers::mcp_server_name(&server).to_string(),
                        plugin_name,
                    )),
                    _ => None,
                })
                .collect(),
                None => Vec::new(),
            }
        };

        let configured_names: std::collections::HashSet<String> = {
            let mcp_state = self.mcp_state.lock().await;
            mcp_state
                .configs
                .iter()
                .map(|c| xai_grok_mcp::servers::mcp_server_name(c).to_string())
                .collect()
        };

        let mut registry = ChannelRegistry::default();
        for raw in raw_entries {
            let spec = match ChannelSpec::parse(&raw) {
                Ok(spec) => spec,
                Err(error) => {
                    tracing::warn!(entry = %raw, %error, "invalid --channels entry");
                    registry.entries.push(ChannelEntry {
                        raw,
                        servers: Vec::new(),
                        state: ChannelState::Invalid(error),
                    });
                    continue;
                }
            };
            let canonical = spec.display();
            if !policy.enabled {
                tracing::warn!(
                    entry = %canonical,
                    "channel blocked: [channels] enabled = false (managed or user config)"
                );
                registry.entries.push(ChannelEntry {
                    raw,
                    servers: Vec::new(),
                    state: ChannelState::BlockedDisabled,
                });
                continue;
            }
            if let Some(allowed) = &policy.allowed
                && !allowed.contains(&canonical)
            {
                tracing::warn!(
                    entry = %canonical,
                    "channel blocked: not in the [channels] allowed list"
                );
                registry.entries.push(ChannelEntry {
                    raw,
                    servers: Vec::new(),
                    state: ChannelState::BlockedNotAllowed,
                });
                continue;
            }
            // For plugin specs, the `@<marketplace>` qualifier is part of
            // the identity: a same-named plugin from another source (e.g.
            // a Claude-compat mirror) must not silently satisfy the
            // opt-in — the user would be channel-enabling a server they
            // did not point at.
            if let ChannelSpec::Plugin { name, marketplace } = &spec {
                let active = plugin_registry
                    .as_ref()
                    .and_then(|reg| reg.get(name))
                    .filter(|p| p.enabled && p.trusted);
                if let Some(plugin) = active
                    && plugin_origin_marketplace(&plugin.origin) != Some(marketplace.as_str())
                {
                    let active_source = describe_plugin_origin(&plugin.origin);
                    tracing::warn!(
                        entry = %canonical, %active_source,
                        "channel entry names a marketplace, but the active plugin with \
                         that name comes from a different source (name shadowing?)"
                    );
                    registry.entries.push(ChannelEntry {
                        raw,
                        servers: Vec::new(),
                        state: ChannelState::MarketplaceMismatch { active_source },
                    });
                    continue;
                }
            }
            let servers: Vec<String> = match &spec {
                ChannelSpec::Server { name } => {
                    if configured_names.contains(name) {
                        vec![name.clone()]
                    } else {
                        Vec::new()
                    }
                }
                ChannelSpec::Plugin { name, .. } => plugin_servers
                    .iter()
                    .filter(|(_, plugin)| plugin == name)
                    .map(|(server, _)| server.clone())
                    .collect(),
            };
            if servers.is_empty() {
                tracing::warn!(
                    entry = %canonical,
                    "channel entry matched no configured MCP server"
                );
                registry.entries.push(ChannelEntry {
                    raw,
                    servers: Vec::new(),
                    state: ChannelState::Unmatched,
                });
                continue;
            }
            tracing::info!(
                entry = %canonical, servers = ?servers,
                "channel registered: events from these servers inject into this session"
            );
            for server in &servers {
                registry
                    .active_servers
                    .insert(server.clone(), canonical.clone());
            }
            registry.entries.push(ChannelEntry {
                raw,
                servers,
                state: ChannelState::Active,
            });
        }

        // Credentials: merge `~/.grok/channels/<server>/.env` into the
        // stdio spawn env of each active channel server. Explicit `env`
        // entries in the server config win over the .env file.
        if !registry.active_servers.is_empty() {
            let mut mcp_state = self.mcp_state.lock().await;
            for config in mcp_state.configs.iter_mut() {
                let acp::McpServer::Stdio(stdio) = config else {
                    continue;
                };
                if !registry.active_servers.contains_key(stdio.name.as_str()) {
                    continue;
                }
                let env_path = channel_env_dir(&stdio.name).join(".env");
                let vars = load_channel_env_file(&env_path);
                if vars.is_empty() {
                    continue;
                }
                let existing: std::collections::HashSet<&str> =
                    stdio.env.iter().map(|e| e.name.as_str()).collect();
                let fresh: Vec<(String, String)> = vars
                    .into_iter()
                    .filter(|(k, _)| !existing.contains(k.as_str()))
                    .collect();
                if fresh.is_empty() {
                    continue;
                }
                tracing::info!(
                    server = %stdio.name, count = fresh.len(), path = %env_path.display(),
                    "loaded channel credentials into server environment"
                );
                for (name, value) in fresh {
                    stdio.env.push(acp::EnvVariable::new(name, value));
                }
            }
        }

        let any_active = !registry.active_servers.is_empty();
        *self
            .channel_registry
            .lock()
            .expect("channel_registry mutex poisoned") = registry;
        any_active
    }

    /// Render `/channels` output: every `--channels` entry with its
    /// resolution state, plus live per-server connection/capability
    /// status for active entries.
    pub(super) async fn build_channels_status(&self) -> String {
        let (entries, active): (Vec<ChannelEntry>, Vec<String>) = {
            let registry = self
                .channel_registry
                .lock()
                .expect("channel_registry mutex poisoned");
            (
                registry.entries.clone(),
                registry.active_servers.keys().cloned().collect(),
            )
        };
        if entries.is_empty() {
            return "No channels enabled for this session.\n\n\
                    Channels let external systems (chat bridges, webhooks, CI alerts) push \
                    events into a running session. Opt in per session by restarting with:\n\
                    \n  grok --channels server:<mcp-server-name>\n  grok --channels plugin:<name>@<marketplace>\n\
                    \nThe named MCP server must declare the `grok/channel` capability. \
                    See the channels page of the user guide for details."
                .to_string();
        }
        // Live status per active server: connected + capability check.
        let mut server_status: std::collections::HashMap<String, &'static str> =
            std::collections::HashMap::new();
        {
            let clients: Vec<(
                String,
                std::sync::Arc<crate::session::mcp_servers::McpClient>,
            )> = {
                let mcp_state = self.mcp_state.lock().await;
                mcp_state
                    .all_clients()
                    .filter(|(name, _)| active.contains(name))
                    .map(|(name, client)| (name.clone(), std::sync::Arc::clone(client)))
                    .collect()
            };
            for (name, client) in clients {
                let status = if client.declares_channel_capability().await {
                    "connected, channel capability declared"
                } else {
                    "connected, but the server does NOT declare the grok/channel capability — it will never push events"
                };
                server_status.insert(name, status);
            }
        }
        let mut lines = vec![format!("Channels ({} entries):", entries.len())];
        for entry in &entries {
            match &entry.state {
                ChannelState::Active => {
                    lines.push(format!("  {} — active", entry.raw));
                    for server in &entry.servers {
                        let status = server_status.get(server.as_str()).copied().unwrap_or(
                            "not connected yet (still initializing, or failed to start)",
                        );
                        lines.push(format!("    {server}: {status}"));
                    }
                }
                ChannelState::BlockedDisabled => lines.push(format!(
                    "  {} — blocked: [channels] enabled = false in config",
                    entry.raw
                )),
                ChannelState::BlockedNotAllowed => lines.push(format!(
                    "  {} — blocked: not in the [channels] allowed list",
                    entry.raw
                )),
                ChannelState::Invalid(error) => {
                    lines.push(format!("  {} — invalid: {error}", entry.raw));
                }
                ChannelState::Unmatched => lines.push(format!(
                    "  {} — matched no configured MCP server (check the name and your MCP config)",
                    entry.raw
                )),
                ChannelState::MarketplaceMismatch { active_source } => lines.push(format!(
                    "  {} — blocked: the active plugin with this name comes from {}, not the \
                     marketplace you named (a same-named plugin is shadowing the one you opted \
                     into; check /plugins and disable or uninstall the other one)",
                    entry.raw, active_source
                )),
            }
        }
        lines.push(String::new());
        lines.push(
            "Events from active channels inject into this session as <channel> messages."
                .to_string(),
        );
        lines.join("\n")
    }

    /// Try to execute an inbound channel event as a host-side slash
    /// command. Returns `true` when the event was consumed — command
    /// recognized, executed, and answered through the originating
    /// server's declared reply tool — so the forwarder must NOT inject
    /// it into the model. Returns `false` for everything else (not a
    /// command, bot-authored, server declared no `commands` descriptor,
    /// unknown command name, or no reply target on the event); those
    /// events flow to the model unchanged, which keeps `/`-prefixed
    /// skill requests working.
    pub(super) async fn try_handle_channel_command(
        &self,
        event: &xai_grok_mcp::channel::ChannelInboundEvent,
    ) -> bool {
        let Some(command) = parse_channel_command(&event.content) else {
            return false;
        };
        // Commands come from humans on the sender allowlist. A
        // bot-authored `/status` is not a host command — it injects
        // like any other message, so the agent can decide what to do.
        if event.meta.iter().any(|(key, _)| key == "bot") {
            return false;
        }
        let client = {
            let mcp_state = self.mcp_state.lock().await;
            mcp_state
                .all_clients()
                .find(|(name, _)| name.as_str() == event.server)
                .map(|(_, client)| std::sync::Arc::clone(client))
        };
        let Some(client) = client else {
            return false;
        };
        // Opt-in: only servers that told the host how to route replies
        // get command handling at all.
        let Some(descriptor) = client.channel_commands_descriptor().await else {
            return false;
        };
        let output = match command.as_str() {
            "help" => channel_command_help(),
            "status" | "session" => self.build_session_info_text().await,
            "channels" => self.build_channels_status().await,
            _ => return false,
        };
        let Some(target) = event
            .meta
            .iter()
            .find(|(key, _)| *key == descriptor.target_meta)
            .map(|(_, value)| value.clone())
        else {
            tracing::warn!(
                server = %event.server, %command, target_meta = %descriptor.target_meta,
                "channel command has no reply target in event meta; injecting as a normal event"
            );
            return false;
        };
        let mut arguments = serde_json::Map::new();
        arguments.insert(descriptor.target_arg.clone(), target.into());
        arguments.insert(descriptor.content_arg.clone(), output.into());
        for (arg, meta_key) in &descriptor.extra_args {
            if let Some((_, value)) = event.meta.iter().find(|(key, _)| key == meta_key) {
                arguments.insert(arg.clone(), value.clone().into());
            }
        }
        tracing::info!(
            server = %event.server, %command, tool = %descriptor.reply_tool,
            "executing channel slash command host-side"
        );
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.call_tool(&descriptor.reply_tool, serde_json::Value::Object(arguments)),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(
                server = %event.server, %command, %error,
                "channel command reply tool call failed"
            ),
            Err(_) => tracing::warn!(
                server = %event.server, %command,
                "channel command reply tool call timed out"
            ),
        }
        // Consumed either way: a command whose reply failed must not
        // fall through to the model as if it were conversation.
        true
    }

    /// Forward inbound channel events into the session as idle-gated
    /// pending notifications. One task per session, spawned by
    /// `run_session` only when `setup_channels` reported an active
    /// entry; exits when the pipe's senders are dropped (session
    /// teardown).
    pub(super) fn spawn_channel_forwarder(
        session: Arc<SessionActor>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
        mut channel_rx: mpsc::UnboundedReceiver<xai_grok_mcp::channel::ChannelInboundEvent>,
    ) {
        tokio::task::spawn_local(async move {
            while let Some(event) = channel_rx.recv().await {
                // Defense in depth: only servers the registry marked
                // active may deliver, even though non-opted-in servers
                // never get a sender wired.
                let active = session
                    .channel_registry
                    .lock()
                    .expect("channel_registry mutex poisoned")
                    .is_active(&event.server);
                if !active {
                    tracing::debug!(
                        server = %event.server,
                        "dropping channel event from non-opted-in server"
                    );
                    continue;
                }
                // Host-executed slash commands (`/status`, `/channels`,
                // `/help`) short-circuit injection: the host answers
                // through the channel itself and the model never sees
                // the event.
                if session.try_handle_channel_command(&event).await {
                    continue;
                }
                let envelope = format_channel_event(&event);
                tracing::info!(
                    server = %event.server, bytes = envelope.len(),
                    "channel event queued for injection"
                );
                {
                    let mut state = session.state.lock().await;
                    state.pending_notifications.push(PendingNotification {
                        prompt_id: format!("channel-{}", uuid::Uuid::now_v7()),
                        prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                            envelope,
                        ))],
                        priority: NotificationPriority::Later,
                        source: NotificationSource::ChannelEvent {
                            server: event.server.clone(),
                        },
                    });
                    if state.pending_notifications.len() > MAX_PENDING_NOTIFICATIONS {
                        let excess = state.pending_notifications.len() - MAX_PENDING_NOTIFICATIONS;
                        state.pending_notifications.drain(..excess);
                        tracing::warn!(
                            dropped = excess,
                            "dropped oldest pending notifications (channel burst exceeded cap of {})",
                            MAX_PENDING_NOTIFICATIONS,
                        );
                    }
                }
                SessionActor::maybe_drain_notifications(session.clone(), completion_tx.clone())
                    .await;
            }
            tracing::debug!("channel forwarder exited (event pipe closed)");
        });
    }
}
