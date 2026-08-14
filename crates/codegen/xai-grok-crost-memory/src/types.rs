//! Wire-independent value types for the Crost memory client.
//!
//! Everything here is provider-agnostic: the `hindsight` and `fake` providers
//! both speak in these types, and so does every layer above them (recall
//! orchestration, outbox, diagnostics).

use serde::Deserialize;
use serde::Serialize;

/// Which bank a recall targets. Private is the calling agent's own bank;
/// shared is the cross-agent bank for the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecallScope {
    Private,
    Shared,
}

impl RecallScope {
    /// Lowercase label used in the injected block and in tracing fields.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Shared => "shared",
        }
    }
}

impl std::fmt::Display for RecallScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One memory returned by a recall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallItem {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub source_agent: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub score: f64,
}

impl RecallItem {
    /// Minimal constructor used by tests and by the fake provider's seeding
    /// helpers.
    pub fn new(id: impl Into<String>, content: impl Into<String>, score: f64) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            kind: None,
            created_at: None,
            source_agent: None,
            task_id: None,
            score,
        }
    }
}

/// A single test invocation and its outcome, recorded as turn evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestEvidence {
    pub cmd: String,
    pub result: String,
}

/// The compact, structured summary of one completed turn.
///
/// Built from typed turn state that the shell already has — never from an
/// extra model call, never from raw transcripts or hidden reasoning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub tests: Vec<TestEvidence>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default)]
    pub next_step: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
}

impl TurnRecord {
    /// False when the turn carries nothing worth remembering — no objective,
    /// no decisions, no files, no tests, no blockers. Retention skips these so
    /// trivial turns ("what time is it?") never reach a bank.
    ///
    /// `next_step`, `task_id`, `branch` and `commit` deliberately do NOT make a
    /// record meaningful on their own: they are ambient turn metadata that is
    /// present on essentially every turn.
    pub fn is_meaningful(&self) -> bool {
        opt_has_content(self.objective.as_deref())
            || list_has_content(&self.decisions)
            || list_has_content(&self.files_changed)
            || self
                .tests
                .iter()
                .any(|t| !t.cmd.trim().is_empty() || !t.result.trim().is_empty())
            || list_has_content(&self.blockers)
    }
}

fn opt_has_content(v: Option<&str>) -> bool {
    v.is_some_and(|s| !s.trim().is_empty())
}

fn list_has_content(v: &[String]) -> bool {
    v.iter().any(|s| !s.trim().is_empty())
}

/// The four kinds of record that may be promoted to the shared bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromoteKind {
    Decision,
    Result,
    Blocker,
    Handoff,
}

impl PromoteKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Result => "result",
            Self::Blocker => "blocker",
            Self::Handoff => "handoff",
        }
    }

    /// All kinds, in schema order.
    pub const ALL: [Self; 4] = [Self::Decision, Self::Result, Self::Blocker, Self::Handoff];

    /// Parse the lowercase wire spelling.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "decision" => Some(Self::Decision),
            "result" => Some(Self::Result),
            "blocker" => Some(Self::Blocker),
            "handoff" => Some(Self::Handoff),
            _ => None,
        }
    }
}

impl std::fmt::Display for PromoteKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Evidence attached to a shared promotion. Decisions and results should
/// carry at least one of these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub test_cmd: Option<String>,
    #[serde(default)]
    pub test_result: Option<String>,
    #[serde(default)]
    pub pr: Option<String>,
}

impl Evidence {
    /// True when at least one evidence field is populated.
    pub fn is_empty(&self) -> bool {
        !(opt_has_content(self.commit.as_deref())
            || opt_has_content(self.test_cmd.as_deref())
            || opt_has_content(self.test_result.as_deref())
            || opt_has_content(self.pr.as_deref()))
    }
}

/// The payload of an explicit shared promotion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteRecord {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub evidence: Evidence,
    #[serde(default)]
    pub next_owner: Option<String>,
    #[serde(default)]
    pub next_action: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
}

/// An automatic retention write, addressed to the agent's private bank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainOp {
    pub op_id: String,
    pub record: TurnRecord,
}

impl RetainOp {
    /// Build an op with a freshly generated, stable `op_id`.
    pub fn new(record: TurnRecord) -> Self {
        Self {
            op_id: new_op_id(),
            record,
        }
    }
}

/// An explicit promotion write, addressed to the shared bank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromoteOp {
    pub op_id: String,
    pub kind: PromoteKind,
    pub record: PromoteRecord,
    /// Id of the record this one corrects. A correction is a NEW record; the
    /// superseded one is never overwritten.
    #[serde(default)]
    pub supersedes: Option<String>,
}

impl PromoteOp {
    /// Build an op with a freshly generated, stable `op_id`.
    pub fn new(kind: PromoteKind, record: PromoteRecord, supersedes: Option<String>) -> Self {
        Self {
            op_id: new_op_id(),
            kind,
            record,
            supersedes,
        }
    }
}

/// Provider health snapshot for diagnostics. Never carries the API token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    pub healthy: bool,
    pub provider: &'static str,
    pub endpoint: Option<String>,
    pub latency_ms: Option<u64>,
    pub detail: Option<String>,
}

/// Prefix on every client-generated operation id.
pub const OP_ID_PREFIX: &str = "cm-";

/// Generate an idempotency key for one logical operation.
///
/// `"cm-" + 128 random bits in hex`. Computed ONCE per operation and stored
/// with it — a retry re-sends the same id, so the server's upsert on
/// `document_id` makes duplicates impossible.
pub fn new_op_id() -> String {
    let hi = fastrand::u64(..);
    let lo = fastrand::u64(..);
    format!("{OP_ID_PREFIX}{hi:016x}{lo:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_id_has_stable_shape() {
        let id = new_op_id();
        assert!(id.starts_with("cm-"), "{id}");
        assert_eq!(id.len(), 3 + 32);
        assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn op_ids_are_distinct() {
        let a = new_op_id();
        let b = new_op_id();
        assert_ne!(a, b);
    }

    #[test]
    fn empty_turn_record_is_not_meaningful() {
        assert!(!TurnRecord::default().is_meaningful());
    }

    #[test]
    fn ambient_metadata_alone_is_not_meaningful() {
        let rec = TurnRecord {
            next_step: Some("keep going".into()),
            task_id: Some("T-1".into()),
            branch: Some("main".into()),
            commit: Some("abc1234".into()),
            ..TurnRecord::default()
        };
        assert!(!rec.is_meaningful());
    }

    #[test]
    fn blank_strings_do_not_make_a_record_meaningful() {
        let rec = TurnRecord {
            objective: Some("   ".into()),
            decisions: vec![String::new(), "  ".into()],
            ..TurnRecord::default()
        };
        assert!(!rec.is_meaningful());
    }

    #[test]
    fn any_substantive_field_makes_a_record_meaningful() {
        let with_objective = TurnRecord {
            objective: Some("ship it".into()),
            ..TurnRecord::default()
        };
        assert!(with_objective.is_meaningful());

        let with_files = TurnRecord {
            files_changed: vec!["src/lib.rs".into()],
            ..TurnRecord::default()
        };
        assert!(with_files.is_meaningful());

        let with_tests = TurnRecord {
            tests: vec![TestEvidence {
                cmd: "cargo test".into(),
                result: "ok".into(),
            }],
            ..TurnRecord::default()
        };
        assert!(with_tests.is_meaningful());

        let with_blockers = TurnRecord {
            blockers: vec!["needs creds".into()],
            ..TurnRecord::default()
        };
        assert!(with_blockers.is_meaningful());
    }

    #[test]
    fn promote_kind_serdes_lowercase() {
        let json = serde_json::to_string(&PromoteKind::Handoff).unwrap();
        assert_eq!(json, "\"handoff\"");
        let back: PromoteKind = serde_json::from_str("\"blocker\"").unwrap();
        assert_eq!(back, PromoteKind::Blocker);
        assert_eq!(PromoteKind::parse("decision"), Some(PromoteKind::Decision));
        assert_eq!(PromoteKind::parse("Decision"), None);
    }

    #[test]
    fn recall_scope_labels() {
        assert_eq!(RecallScope::Private.as_str(), "private");
        assert_eq!(RecallScope::Shared.to_string(), "shared");
    }

    #[test]
    fn evidence_emptiness() {
        assert!(Evidence::default().is_empty());
        assert!(
            !Evidence {
                commit: Some("deadbee".into()),
                ..Evidence::default()
            }
            .is_empty()
        );
        assert!(
            Evidence {
                pr: Some("  ".into()),
                ..Evidence::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn ops_generate_their_own_ids() {
        let retain = RetainOp::new(TurnRecord::default());
        assert!(retain.op_id.starts_with(OP_ID_PREFIX));
        let promote = PromoteOp::new(PromoteKind::Result, PromoteRecord::default(), None);
        assert!(promote.op_id.starts_with(OP_ID_PREFIX));
        assert_ne!(retain.op_id, promote.op_id);
    }
}
