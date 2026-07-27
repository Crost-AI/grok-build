//! Diagnostics for the fork's `status` / `doctor` / `retry` commands.
//!
//! Hard rule: this module reports *about* memory, never memory itself. No
//! recalled content, no record bodies, no token value — only identity, health,
//! counts and latencies.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::CrostMemoryConfig;
use crate::identity::IdentityError;
use crate::identity::ProjectIdentity;
use crate::identity::resolve_project_identity_detailed;
use crate::outbox::Outbox;
use crate::provider::build_provider;
use crate::recall_orchestrator::RecallOutcome;

/// Stats from the most recent pre-turn recall.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecallStats {
    pub private_items: usize,
    pub shared_items: usize,
    pub injected_tokens: usize,
    pub latency_ms: u64,
    pub degraded: bool,
}

impl From<&RecallOutcome> for RecallStats {
    fn from(o: &RecallOutcome) -> Self {
        Self {
            private_items: o.private_n,
            shared_items: o.shared_n,
            injected_tokens: o.injected_tokens,
            latency_ms: o.latency_ms,
            degraded: o.degraded,
        }
    }
}

/// Outcome of the most recent retention attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainStats {
    pub ok: bool,
    /// Short reason when `ok` is false. Never a record body.
    pub detail: Option<String>,
}

/// Everything `doctor` found, in one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDiag {
    pub enabled: bool,
    pub provider: String,
    pub agent_id: String,
    /// Resolved identity, or `None` with `identity_error` explaining why.
    pub identity: Option<ProjectIdentity>,
    pub identity_source: Option<PathBuf>,
    pub identity_error: Option<String>,
    pub endpoint: Option<String>,
    pub endpoint_healthy: Option<bool>,
    pub endpoint_latency_ms: Option<u64>,
    pub endpoint_detail: Option<String>,
    /// NAME of the credential env var, and whether it currently holds a value.
    pub api_key_env: String,
    pub api_key_present: bool,
    pub outbox_dir: Option<PathBuf>,
    pub outbox_depth: usize,
    pub outbox_oldest_age: Option<Duration>,
    pub last_recall: Option<RecallStats>,
    pub last_retain: Option<RetainStats>,
}

/// Gather a diagnostics snapshot, including a live health probe.
///
/// `start_dir` is the session cwd; `outbox_root` is the fork's outbox home
/// (this fork: `~/.grok/crost-memory/outbox`).
pub async fn doctor(
    cfg: &CrostMemoryConfig,
    start_dir: &Path,
    outbox_root: &Path,
    last_recall: Option<RecallStats>,
    last_retain: Option<RetainStats>,
) -> MemoryDiag {
    let agent_id = cfg.resolved_agent_id();
    let mut diag = MemoryDiag {
        enabled: cfg.enabled,
        provider: cfg.provider.to_string(),
        agent_id,
        identity: None,
        identity_source: None,
        identity_error: None,
        endpoint: cfg.base_url.clone(),
        endpoint_healthy: None,
        endpoint_latency_ms: None,
        endpoint_detail: None,
        api_key_env: cfg.api_key_env.clone(),
        api_key_present: cfg.api_key_present(),
        outbox_dir: None,
        outbox_depth: 0,
        outbox_oldest_age: None,
        last_recall,
        last_retain,
    };

    let resolved = match resolve_project_identity_detailed(start_dir, &cfg.project_file) {
        Ok(r) => r,
        Err(e) => {
            diag.identity_error = Some(describe_identity_error(&e));
            return diag;
        }
    };

    let outbox = Outbox::for_project(outbox_root, &resolved.identity.project_id);
    diag.outbox_dir = Some(outbox.dir().to_path_buf());
    diag.outbox_depth = outbox.depth();
    diag.outbox_oldest_age = outbox.oldest_age();

    match build_provider(cfg, &resolved.identity) {
        Ok(provider) => {
            let status = provider.status().await;
            diag.endpoint_healthy = Some(status.healthy);
            diag.endpoint_latency_ms = status.latency_ms;
            diag.endpoint_detail = status.detail;
            if status.endpoint.is_some() {
                diag.endpoint = status.endpoint;
            }
        }
        Err(e) => {
            diag.endpoint_healthy = Some(false);
            diag.endpoint_detail = Some(e.to_string());
        }
    }

    diag.identity = Some(resolved.identity);
    diag.identity_source = Some(resolved.source);
    diag
}

/// Turn a resolution failure into a one-line operator-facing explanation.
fn describe_identity_error(e: &IdentityError) -> String {
    e.to_string()
}

impl MemoryDiag {
    /// Plain-text rendering for the fork's status command.
    pub fn render(&self) -> String {
        let mut out = String::from("Crost project memory\n");
        out.push_str(&format!(
            "  enabled:    {}\n",
            if self.enabled { "yes" } else { "no" }
        ));
        out.push_str(&format!("  provider:   {}\n", self.provider));
        out.push_str(&format!("  agent id:   {}\n", self.agent_id));

        match (&self.identity, &self.identity_error) {
            (Some(id), _) => {
                out.push_str(&format!("  project:    {} ({})\n", id.slug, id.project_id));
                if let Some(prefix) = &id.bank_prefix {
                    out.push_str(&format!("  bank prefix:{prefix}\n"));
                }
                if let Some(src) = &self.identity_source {
                    out.push_str(&format!("  identity:   {}\n", src.display()));
                }
            }
            (None, Some(reason)) => {
                out.push_str(&format!("  project:    none — {reason}\n"));
            }
            (None, None) => out.push_str("  project:    none\n"),
        }

        out.push_str(&format!(
            "  endpoint:   {}\n",
            self.endpoint.as_deref().unwrap_or("(unset)")
        ));
        let health = match self.endpoint_healthy {
            Some(true) => "reachable".to_string(),
            Some(false) => format!(
                "UNREACHABLE{}",
                self.endpoint_detail
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default()
            ),
            None => "not checked".to_string(),
        };
        let latency = self
            .endpoint_latency_ms
            .map(|ms| format!(" ({ms} ms)"))
            .unwrap_or_default();
        out.push_str(&format!("  health:     {health}{latency}\n"));
        out.push_str(&format!(
            "  api key:    ${} is {}\n",
            self.api_key_env,
            if self.api_key_present { "set" } else { "unset" }
        ));

        out.push_str(&format!("  outbox:     {} queued", self.outbox_depth));
        if let Some(age) = self.outbox_oldest_age {
            out.push_str(&format!(", oldest {}", humanize(age)));
        }
        out.push('\n');
        if let Some(dir) = &self.outbox_dir {
            out.push_str(&format!("              {}\n", dir.display()));
        }

        match &self.last_recall {
            Some(r) => out.push_str(&format!(
                "  last recall:{} private / {} shared, ~{} tokens, {} ms{}\n",
                r.private_items,
                r.shared_items,
                r.injected_tokens,
                r.latency_ms,
                if r.degraded { " (degraded)" } else { "" }
            )),
            None => out.push_str("  last recall: none this session\n"),
        }
        match &self.last_retain {
            Some(r) if r.ok => out.push_str("  last retain: ok\n"),
            Some(r) => out.push_str(&format!(
                "  last retain: failed{}\n",
                r.detail
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default()
            )),
            None => out.push_str("  last retain: none this session\n"),
        }
        out
    }
}

fn humanize(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;
    use crate::types::RetainOp;
    use crate::types::TurnRecord;

    const PROJECT_YAML: &str =
        "apiVersion: memory.crost/v1\nprojectId: p-123\nslug: ohm-storefront\n";

    fn write_project(dir: &Path) {
        let crost = dir.join(".crost");
        std::fs::create_dir_all(&crost).unwrap();
        std::fs::write(crost.join("project.yaml"), PROJECT_YAML).unwrap();
    }

    fn fake_cfg() -> CrostMemoryConfig {
        CrostMemoryConfig {
            enabled: true,
            provider: ProviderKind::Fake,
            api_key_env: "CROST_TEST_DEFINITELY_UNSET_KEY_ENV".into(),
            ..CrostMemoryConfig::default()
        }
    }

    #[tokio::test]
    async fn doctor_reports_a_resolved_project() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let home = tempfile::tempdir().unwrap();
        let diag = doctor(&fake_cfg(), tmp.path(), home.path(), None, None).await;
        let id = diag.identity.as_ref().expect("identity resolved");
        assert_eq!(id.slug, "ohm-storefront");
        assert_eq!(id.project_id, "p-123");
        assert_eq!(diag.identity_error, None);
        assert_eq!(diag.endpoint_healthy, Some(true));
        assert_eq!(diag.agent_id, "grok");
        assert!(!diag.api_key_present);
        assert!(diag.identity_source.is_some());
    }

    #[tokio::test]
    async fn doctor_explains_a_missing_project_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let diag = doctor(&fake_cfg(), tmp.path(), home.path(), None, None).await;
        assert!(diag.identity.is_none());
        let reason = diag.identity_error.as_deref().unwrap_or_default();
        assert!(reason.contains(".crost/project.yaml"), "{reason}");
        let rendered = diag.render();
        assert!(rendered.contains("project:    none —"), "{rendered}");
    }

    #[tokio::test]
    async fn doctor_explains_an_invalid_project_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".crost")).unwrap();
        std::fs::write(
            tmp.path().join(".crost").join("project.yaml"),
            "projectId: p\nslug: Not Valid\n",
        )
        .unwrap();
        let home = tempfile::tempdir().unwrap();
        let diag = doctor(&fake_cfg(), tmp.path(), home.path(), None, None).await;
        let reason = diag.identity_error.as_deref().unwrap_or_default();
        assert!(reason.contains("invalid slug"), "{reason}");
    }

    #[tokio::test]
    async fn doctor_reports_outbox_depth() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let home = tempfile::tempdir().unwrap();
        let outbox = Outbox::for_project(home.path(), "p-123");
        outbox
            .enqueue_retain(&RetainOp::new(TurnRecord {
                objective: Some("x".into()),
                ..TurnRecord::default()
            }))
            .unwrap();
        let diag = doctor(&fake_cfg(), tmp.path(), home.path(), None, None).await;
        assert_eq!(diag.outbox_depth, 1);
        assert!(diag.outbox_oldest_age.is_some());
        assert!(diag.render().contains("1 queued"), "{}", diag.render());
    }

    #[tokio::test]
    async fn doctor_reports_a_provider_that_cannot_be_built() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let home = tempfile::tempdir().unwrap();
        // Hindsight configured without a base_url: the failure is knowable
        // without a single packet, and doctor must say so rather than claim
        // health it never checked.
        let cfg = CrostMemoryConfig {
            enabled: true,
            api_key_env: "CROST_TEST_DEFINITELY_UNSET_KEY_ENV".into(),
            ..CrostMemoryConfig::default()
        };
        let diag = doctor(&cfg, tmp.path(), home.path(), None, None).await;
        assert_eq!(diag.endpoint_healthy, Some(false));
        assert!(
            diag.endpoint_detail
                .as_deref()
                .unwrap_or_default()
                .contains("base_url")
        );
        assert!(diag.render().contains("UNREACHABLE"));
        // Identity still resolved — a bad endpoint is not a bad project.
        assert!(diag.identity.is_some());
    }

    #[test]
    fn render_never_leaks_a_token_or_a_memory_body() {
        let diag = MemoryDiag {
            enabled: true,
            provider: "hindsight".into(),
            agent_id: "grok".into(),
            identity: Some(ProjectIdentity {
                project_id: "p-123".into(),
                slug: "ohm-storefront".into(),
                bank_prefix: None,
            }),
            identity_source: Some(PathBuf::from("/repo/.crost/project.yaml")),
            identity_error: None,
            endpoint: Some("http://hindsight:8080".into()),
            endpoint_healthy: Some(true),
            endpoint_latency_ms: Some(12),
            endpoint_detail: None,
            api_key_env: "HINDSIGHT_API_KEY".into(),
            api_key_present: true,
            outbox_dir: Some(PathBuf::from("/home/u/.grok/crost-memory/outbox/p-123")),
            outbox_depth: 3,
            outbox_oldest_age: Some(Duration::from_secs(4000)),
            last_recall: Some(RecallStats {
                private_items: 2,
                shared_items: 1,
                injected_tokens: 340,
                latency_ms: 88,
                degraded: false,
            }),
            last_retain: Some(RetainStats {
                ok: true,
                detail: None,
            }),
        };
        let out = diag.render();
        assert!(out.contains("$HINDSIGHT_API_KEY is set"), "{out}");
        assert!(out.contains("ohm-storefront"));
        assert!(out.contains("3 queued, oldest 1h"));
        assert!(out.contains("2 private / 1 shared, ~340 tokens, 88 ms"));
        assert!(out.contains("last retain: ok"));
        // The env var NAME appears; no value ever could.
        assert!(!out.contains("Bearer"), "{out}");
    }

    #[test]
    fn render_marks_a_degraded_recall_and_a_failed_retain() {
        let diag = MemoryDiag {
            enabled: false,
            provider: "fake".into(),
            agent_id: "grok".into(),
            identity: None,
            identity_source: None,
            identity_error: Some("no project file".into()),
            endpoint: None,
            endpoint_healthy: None,
            endpoint_latency_ms: None,
            endpoint_detail: None,
            api_key_env: "HINDSIGHT_API_KEY".into(),
            api_key_present: false,
            outbox_dir: None,
            outbox_depth: 0,
            outbox_oldest_age: None,
            last_recall: Some(RecallStats {
                degraded: true,
                ..RecallStats::default()
            }),
            last_retain: Some(RetainStats {
                ok: false,
                detail: Some("backend unavailable".into()),
            }),
        };
        let out = diag.render();
        assert!(out.contains("(degraded)"), "{out}");
        assert!(
            out.contains("last retain: failed — backend unavailable"),
            "{out}"
        );
        assert!(out.contains("enabled:    no"), "{out}");
        assert!(out.contains("endpoint:   (unset)"), "{out}");
        assert!(out.contains("health:     not checked"), "{out}");
    }

    #[test]
    fn recall_stats_convert_from_an_outcome() {
        let outcome = RecallOutcome {
            block: Some("<crost-memory ...>".into()),
            private_n: 2,
            shared_n: 3,
            injected_tokens: 99,
            latency_ms: 12,
            degraded: true,
        };
        let stats = RecallStats::from(&outcome);
        assert_eq!(stats.private_items, 2);
        assert_eq!(stats.shared_items, 3);
        assert_eq!(stats.injected_tokens, 99);
        assert!(stats.degraded);
    }

    #[test]
    fn ages_are_humanized() {
        assert_eq!(humanize(Duration::from_secs(5)), "5s");
        assert_eq!(humanize(Duration::from_secs(300)), "5m");
        assert_eq!(humanize(Duration::from_secs(7200)), "2h");
        assert_eq!(humanize(Duration::from_secs(200_000)), "2d");
    }
}
