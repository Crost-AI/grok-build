//! Crost project-memory client.
//!
//! A leaf crate implementing the Crost Project Memory contract (v1) for this
//! fork: pre-turn recall, post-turn retention, explicit shared promotion, and
//! the diagnostics behind `status` / `doctor` / `retry`.
//!
//! # Shape
//!
//! ```text
//! shell  ──▶ run_recall ──┐
//!                         ├──▶ MemoryProvider ──▶ Hindsight
//!        ──▶ Outbox    ───┘         (or the fake, in tests)
//! ```
//!
//! [`provider::MemoryProvider`] is the only layer that knows Hindsight exists;
//! bank names, URLs and wire bodies live behind it in [`hindsight`]. Everything
//! else — identity, orchestration, redaction, the outbox, diagnostics — is
//! provider-agnostic, which is why [`fake::FakeProvider`] is a complete
//! substitute for the server in tests.
//!
//! # Guarantees this crate is responsible for
//!
//! - **Fail-open.** [`recall_orchestrator::run_recall`] never returns `Err`; a
//!   dead backend costs the turn nothing but a `degraded` flag.
//! - **One scope never sinks the other.** Private and shared recalls run
//!   concurrently under independent deadlines.
//! - **No duplicates.** Every write carries a client-generated `op_id`, created
//!   once and reused verbatim on every retry, which the server upserts on.
//! - **No secrets on disk or on the wire.** [`redact::redact`] runs over every
//!   payload string at enqueue time, before the outbox writes anything.
//! - **Nothing blocks a turn.** Writes land in the [`outbox::Outbox`] and drain
//!   from a detached task.
//!
//! Session wiring (when to call recall, what to put in a [`TurnRecord`], where
//! to mount the promote tool) belongs to the shell and is deliberately not
//! here.

pub mod config;
pub mod diag;
pub mod fake;
pub mod hindsight;
pub mod identity;
pub mod outbox;
pub mod promote_tool;
pub mod provider;
pub mod recall_orchestrator;
pub mod redact;
pub mod types;

pub use config::AGENT_ID_ENV;
pub use config::CrostMemoryConfig;
pub use config::DEFAULT_AGENT_ID;
pub use config::DEFAULT_API_KEY_ENV;
pub use config::ProviderKind;
pub use config::SecretToken;
pub use diag::MemoryDiag;
pub use diag::RecallStats;
pub use diag::RetainStats;
pub use diag::doctor;
pub use fake::FakeProvider;
pub use hindsight::HindsightProvider;
pub use identity::DEFAULT_PROJECT_FILE;
pub use identity::IdentityError;
pub use identity::ProjectIdentity;
pub use identity::resolve_project_identity;
pub use identity::resolve_project_identity_at;
pub use identity::resolve_project_identity_detailed;
pub use outbox::FlushOutcome;
pub use outbox::Outbox;
pub use outbox::OutboxOp;
pub use promote_tool::PROMOTE_TOOL_DESCRIPTION;
pub use promote_tool::PROMOTE_TOOL_NAME;
pub use promote_tool::parse_promote_input;
pub use promote_tool::promote_input_schema;
pub use provider::MemoryError;
pub use provider::MemoryProvider;
pub use provider::build_provider;
pub use recall_orchestrator::RecallOutcome;
pub use recall_orchestrator::run_recall;
pub use redact::redact;
pub use types::Evidence;
pub use types::PromoteKind;
pub use types::PromoteOp;
pub use types::PromoteRecord;
pub use types::ProviderStatus;
pub use types::RecallItem;
pub use types::RecallScope;
pub use types::RetainOp;
pub use types::TestEvidence;
pub use types::TurnRecord;
pub use types::new_op_id;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    fn write_project(dir: &Path) {
        let crost = dir.join(".crost");
        std::fs::create_dir_all(&crost).unwrap();
        std::fs::write(
            crost.join("project.yaml"),
            "apiVersion: memory.crost/v1\nprojectId: p-777\nslug: demo\n",
        )
        .unwrap();
    }

    /// The full lifecycle a shell drives, against the fake provider: resolve,
    /// recall, retain through the outbox, promote, then report.
    #[tokio::test]
    async fn end_to_end_lifecycle_over_the_public_api() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_project(repo.path());

        let cfg = CrostMemoryConfig {
            enabled: true,
            provider: ProviderKind::Fake,
            api_key_env: "CROST_TEST_DEFINITELY_UNSET_KEY_ENV".into(),
            ..CrostMemoryConfig::default()
        };
        let identity = resolve_project_identity(repo.path()).expect("identity resolves");
        assert_eq!(identity.slug, "demo");

        // The shell holds the fake so it can both drive and assert.
        let fake = FakeProvider::new();
        fake.seed(
            RecallScope::Shared,
            vec![RecallItem {
                source_agent: Some("codex".into()),
                created_at: Some("2026-07-12T09:00:00Z".into()),
                ..RecallItem::new("s1", "sqlite was chosen for the cache", 0.9)
            }],
        );
        let provider: Arc<dyn MemoryProvider> = Arc::new(fake.clone());

        // Pre-turn.
        let outcome = run_recall(provider.as_ref(), &cfg, &identity, "why sqlite?").await;
        let block = outcome.block.as_deref().unwrap_or_default();
        assert!(block.starts_with("<crost-memory agent=\"grok\" project=\"demo\""));
        assert!(block.contains("sqlite was chosen for the cache"));
        assert!(!outcome.degraded);

        // Post-turn retention, via the outbox.
        let outbox = Outbox::for_project(home.path(), &identity.project_id);
        let record = TurnRecord {
            objective: Some("add the crost memory crate".into()),
            files_changed: vec!["crates/codegen/xai-grok-crost-memory/src/lib.rs".into()],
            tests: vec![TestEvidence {
                cmd: "cargo test -p xai-grok-crost-memory".into(),
                result: "all passed".into(),
            }],
            ..TurnRecord::default()
        };
        assert!(record.is_meaningful());
        let retain = RetainOp::new(record);
        outbox.enqueue_retain(&retain).unwrap();

        // Explicit promotion, from a tool call.
        let (kind, promote_record, supersedes) = parse_promote_input(serde_json::json!({
            "kind": "result",
            "title": "Crost memory crate landed",
            "summary": "Recall, retain and promote all covered by unit tests.",
            "evidence": { "test_cmd": "cargo test -p xai-grok-crost-memory", "test_result": "green" }
        }))
        .expect("valid tool input");
        assert_eq!(kind, PromoteKind::Result);
        outbox
            .enqueue_promote(&PromoteOp::new(kind, promote_record, supersedes))
            .unwrap();
        assert_eq!(outbox.depth(), 2);

        // Drain.
        let flush = outbox.flush(provider.as_ref()).await;
        assert_eq!(flush.sent, 2);
        assert_eq!(outbox.depth(), 0);
        assert_eq!(fake.retained().len(), 1);
        assert_eq!(fake.promoted().len(), 1);
        assert_eq!(fake.retained()[0].op_id, retain.op_id);

        // Report.
        let diag = doctor(
            &cfg,
            repo.path(),
            home.path(),
            Some(RecallStats::from(&outcome)),
            Some(RetainStats {
                ok: true,
                detail: None,
            }),
        )
        .await;
        assert_eq!(diag.outbox_depth, 0);
        assert_eq!(
            diag.identity.as_ref().map(|i| i.slug.as_str()),
            Some("demo")
        );
        let rendered = diag.render();
        assert!(rendered.contains("last retain: ok"), "{rendered}");
        // Diagnostics report about memory, never memory itself.
        assert!(!rendered.contains("sqlite was chosen"), "{rendered}");
    }

    #[tokio::test]
    async fn a_disabled_project_produces_no_identity_and_no_memory() {
        let empty = tempfile::tempdir().unwrap();
        assert!(resolve_project_identity(empty.path()).is_none());
    }

    #[test]
    fn the_promote_tool_name_is_the_contract_name() {
        assert_eq!(PROMOTE_TOOL_NAME, "crost_memory_promote_shared");
    }
}
