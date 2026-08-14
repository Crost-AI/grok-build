//! Client configuration.
//!
//! Credentials are referenced by env-var NAME only. The token value is read at
//! client construction and wrapped in [`SecretToken`], whose `Debug` prints
//! `***` — so it cannot leak through a `#[derive(Debug)]` on any struct that
//! transitively holds it.

use serde::Deserialize;
use serde::Serialize;

use crate::identity::DEFAULT_PROJECT_FILE;

/// Env var that overrides the configured agent id.
pub const AGENT_ID_ENV: &str = "CROST_AGENT_ID";

/// Default env var holding the Hindsight API key.
pub const DEFAULT_API_KEY_ENV: &str = "HINDSIGHT_API_KEY";

/// This fork's agent id.
pub const DEFAULT_AGENT_ID: &str = "grok";

/// Which backing implementation to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// The production driver, talking to a Hindsight deployment.
    #[default]
    Hindsight,
    /// In-memory double for tests.
    Fake,
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hindsight => "hindsight",
            Self::Fake => "fake",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything the memory client needs, with contract defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CrostMemoryConfig {
    /// Master switch. Off by default: a fork that ships this crate does not
    /// start talking to a server until an operator opts in.
    pub enabled: bool,
    pub provider: ProviderKind,
    /// Hindsight base URL. Required when `provider = hindsight`.
    pub base_url: Option<String>,
    /// Selects the private bank and labels shared records.
    pub agent_id: String,
    /// NAME of the env var holding the API key — never the key itself.
    pub api_key_env: String,
    /// Identity file path, relative to a repo ancestor directory.
    pub project_file: String,
    /// Hard per-scope recall deadline.
    pub recall_timeout_ms: u64,
    /// Max items injected per scope.
    pub recall_max_items: usize,
    pub private_token_budget: usize,
    pub shared_token_budget: usize,
    /// Automatic post-turn retention into the private bank.
    pub retain_enabled: bool,
    /// Whether the `crost_memory_promote_shared` tool is offered.
    pub shared_promotion_enabled: bool,
    /// Failures degrade to "no memory" rather than surfacing to the model.
    pub fail_open: bool,
}

impl Default for CrostMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: ProviderKind::Hindsight,
            base_url: None,
            agent_id: DEFAULT_AGENT_ID.to_string(),
            api_key_env: DEFAULT_API_KEY_ENV.to_string(),
            project_file: DEFAULT_PROJECT_FILE.to_string(),
            recall_timeout_ms: 1500,
            recall_max_items: 8,
            private_token_budget: 1200,
            shared_token_budget: 800,
            retain_enabled: true,
            shared_promotion_enabled: true,
            fail_open: true,
        }
    }
}

impl CrostMemoryConfig {
    /// Agent id after applying the `CROST_AGENT_ID` override.
    pub fn resolved_agent_id(&self) -> String {
        resolve_agent_id(&self.agent_id, std::env::var(AGENT_ID_ENV).ok().as_deref())
    }

    /// Read the API token from the env var named by `api_key_env`.
    ///
    /// `None` is normal — Hindsight may run authless — and is not an error.
    pub fn load_token(&self) -> Option<SecretToken> {
        std::env::var(&self.api_key_env)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(SecretToken::new)
    }

    /// Whether the configured key env var currently holds a value. For
    /// diagnostics only — the value itself never leaves [`SecretToken`].
    pub fn api_key_present(&self) -> bool {
        self.load_token().is_some()
    }

    /// Per-scope token budget.
    pub fn token_budget(&self, scope: crate::types::RecallScope) -> usize {
        match scope {
            crate::types::RecallScope::Private => self.private_token_budget,
            crate::types::RecallScope::Shared => self.shared_token_budget,
        }
    }

    /// The recall deadline as a `Duration`.
    pub fn recall_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.recall_timeout_ms)
    }
}

/// Pure form of the agent-id override, so the rule is testable without
/// mutating process environment.
fn resolve_agent_id(configured: &str, env_override: Option<&str>) -> String {
    env_override
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| configured.trim().to_string())
}

/// An API token that refuses to print itself.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only way to get the raw value; used solely to build the
    /// `Authorization` header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

impl std::fmt::Display for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RecallScope;

    #[test]
    fn defaults_match_the_contract() {
        let c = CrostMemoryConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.provider, ProviderKind::Hindsight);
        assert_eq!(c.base_url, None);
        assert_eq!(c.agent_id, "grok");
        assert_eq!(c.api_key_env, "HINDSIGHT_API_KEY");
        assert_eq!(c.project_file, ".crost/project.yaml");
        assert_eq!(c.recall_timeout_ms, 1500);
        assert_eq!(c.recall_max_items, 8);
        assert_eq!(c.private_token_budget, 1200);
        assert_eq!(c.shared_token_budget, 800);
        assert!(c.retain_enabled);
        assert!(c.shared_promotion_enabled);
        assert!(c.fail_open);
    }

    #[test]
    fn token_budget_is_per_scope() {
        let c = CrostMemoryConfig::default();
        assert_eq!(c.token_budget(RecallScope::Private), 1200);
        assert_eq!(c.token_budget(RecallScope::Shared), 800);
    }

    #[test]
    fn env_overrides_agent_id() {
        assert_eq!(resolve_agent_id("grok", Some("codex")), "codex");
        assert_eq!(resolve_agent_id("grok", Some("  claude ")), "claude");
    }

    #[test]
    fn blank_or_absent_env_keeps_configured_agent_id() {
        assert_eq!(resolve_agent_id("grok", None), "grok");
        assert_eq!(resolve_agent_id("grok", Some("")), "grok");
        assert_eq!(resolve_agent_id("grok", Some("   ")), "grok");
    }

    #[test]
    fn secret_token_never_prints_itself() {
        let t = SecretToken::new("hs-super-secret-value");
        assert_eq!(format!("{t:?}"), "***");
        assert_eq!(format!("{t}"), "***");
        assert!(!format!("{t:?}").contains("super"));
        assert_eq!(t.expose(), "hs-super-secret-value");
    }

    #[test]
    fn nested_debug_does_not_leak_the_token() {
        #[derive(Debug)]
        struct Holder {
            token: Option<SecretToken>,
        }
        let h = Holder {
            token: Some(SecretToken::new("abc123")),
        };
        assert_eq!(h.token.as_ref().map(SecretToken::expose), Some("abc123"));
        let rendered = format!("{h:?}");
        assert!(!rendered.contains("abc123"), "{rendered}");
        assert!(rendered.contains("***"));
    }

    #[test]
    fn config_round_trips_through_json_with_defaults() {
        let json = serde_json::json!({ "enabled": true, "base_url": "http://h:8080" });
        let c: CrostMemoryConfig = serde_json::from_value(json).unwrap();
        assert!(c.enabled);
        assert_eq!(c.base_url.as_deref(), Some("http://h:8080"));
        // Unspecified keys fall back to contract defaults.
        assert_eq!(c.recall_timeout_ms, 1500);
        assert_eq!(c.provider, ProviderKind::Hindsight);
    }

    #[test]
    fn provider_kind_serdes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::Fake).unwrap(),
            "\"fake\""
        );
        let p: ProviderKind = serde_json::from_str("\"hindsight\"").unwrap();
        assert_eq!(p, ProviderKind::Hindsight);
        assert_eq!(ProviderKind::Fake.to_string(), "fake");
    }

    #[test]
    fn missing_key_env_var_is_not_an_error() {
        let c = CrostMemoryConfig {
            api_key_env: "CROST_TEST_DEFINITELY_UNSET_KEY_ENV".to_string(),
            ..CrostMemoryConfig::default()
        };
        assert!(c.load_token().is_none());
        assert!(!c.api_key_present());
    }
}
