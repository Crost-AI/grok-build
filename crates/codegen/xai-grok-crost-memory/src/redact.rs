//! Secret redaction, applied to every string field of every payload before it
//! is enqueued — not before it is sent. Redacting at enqueue means the secret
//! never lands on disk in the outbox either.
//!
//! Deliberately conservative: the generic `key = value` pattern will sometimes
//! redact an ordinary assignment whose right-hand side merely *looks* like a
//! credential (`token = fetch_token()`). Losing a line of turn summary is a far
//! cheaper mistake than shipping a live key to a shared memory bank.

use std::borrow::Cow;
use std::sync::OnceLock;

use regex::Regex;

use crate::outbox::OutboxOp;
use crate::types::Evidence;
use crate::types::PromoteOp;
use crate::types::PromoteRecord;
use crate::types::RetainOp;
use crate::types::TestEvidence;
use crate::types::TurnRecord;

/// `(pattern, label)`, applied in order. Order matters: multi-line structures
/// (PEM) and composite tokens (JWT) come first so a later, narrower pattern
/// cannot chew a hole in the middle of them.
const PATTERNS: &[(&str, &str)] = &[
    (
        r"-----BEGIN[A-Z ]*PRIVATE KEY-----(?s).*?-----END[A-Z ]*PRIVATE KEY-----",
        "private_key",
    ),
    (
        r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        "jwt",
    ),
    (r"AKIA[0-9A-Z]{16}", "aws_access_key"),
    (r"ghp_[A-Za-z0-9]{36,}", "github_token"),
    (r"github_pat_[A-Za-z0-9_]{22,}", "github_pat"),
    (r"xox[baprs]-[A-Za-z0-9-]{10,}", "slack_token"),
    (r"sk-[A-Za-z0-9_-]{20,}", "openai_key"),
    (r"(?i)bearer\s+[a-z0-9._~+/-]{16,}=*", "bearer_token"),
    (
        r#"(?i)\b(api[_-]?key|secret|token|password|passwd)\b\s*[=:]\s*['"]?\S{6,}"#,
        "generic_secret",
    ),
];

fn compiled() -> &'static [(Regex, &'static str)] {
    static COMPILED: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        PATTERNS
            .iter()
            .filter_map(|(src, label)| match Regex::new(src) {
                Ok(re) => Some((re, *label)),
                Err(e) => {
                    tracing::error!(pattern = label, error = %e, "crost-memory redaction pattern failed to compile");
                    None
                }
            })
            .collect()
    })
}

/// Replace every recognized credential in `text` with `[REDACTED:<label>]`.
///
/// Returns `Cow::Borrowed` untouched when nothing matched, so the common case
/// allocates nothing.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    for (re, label) in compiled() {
        if !re.is_match(out.as_ref()) {
            continue;
        }
        let replacement = format!("[REDACTED:{label}]");
        let replaced = re
            .replace_all(out.as_ref(), regex::NoExpand(replacement.as_str()))
            .into_owned();
        out = Cow::Owned(replaced);
    }
    out
}

fn redact_owned(s: &str) -> String {
    redact(s).into_owned()
}

fn redact_opt(v: &Option<String>) -> Option<String> {
    v.as_deref().map(redact_owned)
}

fn redact_list(v: &[String]) -> Vec<String> {
    v.iter().map(|s| redact_owned(s)).collect()
}

/// Redact every string field of a turn record.
pub fn redact_turn_record(rec: &TurnRecord) -> TurnRecord {
    TurnRecord {
        objective: redact_opt(&rec.objective),
        decisions: redact_list(&rec.decisions),
        files_changed: redact_list(&rec.files_changed),
        tests: rec
            .tests
            .iter()
            .map(|t| TestEvidence {
                cmd: redact_owned(&t.cmd),
                result: redact_owned(&t.result),
            })
            .collect(),
        blockers: redact_list(&rec.blockers),
        next_step: redact_opt(&rec.next_step),
        task_id: redact_opt(&rec.task_id),
        branch: redact_opt(&rec.branch),
        commit: redact_opt(&rec.commit),
    }
}

/// Redact every string field of a promotion record.
pub fn redact_promote_record(rec: &PromoteRecord) -> PromoteRecord {
    PromoteRecord {
        title: redact_owned(&rec.title),
        summary: redact_owned(&rec.summary),
        status: redact_opt(&rec.status),
        decisions: redact_list(&rec.decisions),
        files: redact_list(&rec.files),
        evidence: Evidence {
            commit: redact_opt(&rec.evidence.commit),
            test_cmd: redact_opt(&rec.evidence.test_cmd),
            test_result: redact_opt(&rec.evidence.test_result),
            pr: redact_opt(&rec.evidence.pr),
        },
        next_owner: redact_opt(&rec.next_owner),
        next_action: redact_opt(&rec.next_action),
        task_id: redact_opt(&rec.task_id),
    }
}

/// Redact a retain op, preserving its `op_id` (idempotency must survive).
pub fn redact_retain_op(op: &RetainOp) -> RetainOp {
    RetainOp {
        op_id: op.op_id.clone(),
        record: redact_turn_record(&op.record),
    }
}

/// Redact a promote op, preserving its `op_id`.
pub fn redact_promote_op(op: &PromoteOp) -> PromoteOp {
    PromoteOp {
        op_id: op.op_id.clone(),
        kind: op.kind,
        record: redact_promote_record(&op.record),
        supersedes: redact_opt(&op.supersedes),
    }
}

/// Redact whichever op this is.
pub fn redact_outbox_op(op: &OutboxOp) -> OutboxOp {
    match op {
        OutboxOp::Retain(o) => OutboxOp::Retain(redact_retain_op(o)),
        OutboxOp::Promote(o) => OutboxOp::Promote(redact_promote_op(o)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_compiles() {
        assert_eq!(compiled().len(), PATTERNS.len());
    }

    fn assert_redacted(input: &str, secret: &str, label: &str) {
        let out = redact(input);
        assert!(
            !out.contains(secret),
            "secret survived redaction: {out} (label {label})"
        );
        assert!(
            out.contains(&format!("[REDACTED:{label}]")),
            "expected label {label} in {out}"
        );
    }

    #[test]
    fn redacts_aws_access_keys() {
        let key = "AKIAIOSFODNN7EXAMPLE";
        assert_redacted("creds AKIAIOSFODNN7EXAMPLE here", key, "aws_access_key");
    }

    #[test]
    fn redacts_github_classic_tokens() {
        let tok = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        assert_redacted(&format!("push with {tok}"), tok, "github_token");
    }

    #[test]
    fn redacts_github_fine_grained_pats() {
        let tok = "github_pat_11ABCDEFG0123456789_abcdefghijklmnop";
        assert_redacted(&format!("token {tok}"), tok, "github_pat");
    }

    #[test]
    fn redacts_generic_key_assignments() {
        assert_redacted(
            "config had api_key = 'hunter2hunter2'",
            "hunter2hunter2",
            "generic_secret",
        );
        assert_redacted("password: correcthorse", "correcthorse", "generic_secret");
        assert_redacted("SECRET=abcdef123456", "abcdef123456", "generic_secret");
    }

    #[test]
    fn redacts_slack_tokens() {
        let tok = "xoxb-123456789012-abcdefghijkl";
        assert_redacted(&format!("slack {tok}"), tok, "slack_token");
    }

    #[test]
    fn redacts_openai_style_keys() {
        let tok = "sk-abcdefghijklmnopqrstuvwxyz0123";
        assert_redacted(&format!("key {tok}"), tok, "openai_key");
    }

    #[test]
    fn redacts_bearer_headers() {
        let tok = "abcdefghijklmnopqrstuvwxyz012345";
        assert_redacted(&format!("Authorization: Bearer {tok}"), tok, "bearer_token");
    }

    #[test]
    fn redacts_jwts() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_redacted(&format!("cookie {jwt}"), jwt, "jwt");
    }

    #[test]
    fn redacts_pem_private_key_blocks() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAum\nc29tZSBrZXkgbWF0ZXJpYWw=\n-----END RSA PRIVATE KEY-----";
        let input = format!("deploy key:\n{pem}\nend");
        let out = redact(&input);
        assert!(!out.contains("MIIEowIBAAKCAQEAum"), "{out}");
        assert!(out.contains("[REDACTED:private_key]"), "{out}");
        assert!(out.contains("deploy key:"));
        assert!(out.contains("end"));
    }

    #[test]
    fn ordinary_prose_and_code_are_untouched() {
        let text = "Refactored the retry loop in src/outbox.rs so a token bucket \
                    limits flush concurrency. The password reset flow is unchanged. \
                    fn compute_total(items: &[Item]) -> u64 { items.iter().map(|i| i.n).sum() } \
                    See PR #412 and commit 9f3a1c2 for the secret sauce.";
        let out = redact(text);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "unexpected redaction: {out}"
        );
        assert_eq!(out, text);
    }

    #[test]
    fn unmatched_text_is_borrowed_not_copied() {
        let out = redact("plain summary");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn multiple_secrets_in_one_string_all_go() {
        let text = "AKIAIOSFODNN7EXAMPLE and ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let out = redact(text);
        assert!(out.contains("[REDACTED:aws_access_key]"));
        assert!(out.contains("[REDACTED:github_token]"));
        assert!(!out.contains("AKIA"));
        assert!(!out.contains("ghp_"));
    }

    #[test]
    fn turn_records_are_redacted_field_by_field() {
        let rec = TurnRecord {
            objective: Some("rotate AKIAIOSFODNN7EXAMPLE".into()),
            decisions: vec!["use ghp_abcdefghijklmnopqrstuvwxyz0123456789".into()],
            files_changed: vec!["src/ok.rs".into()],
            tests: vec![TestEvidence {
                cmd: "curl -H 'Authorization: Bearer abcdefghijklmnopqrstuvwx'".into(),
                result: "ok".into(),
            }],
            blockers: vec!["needs password: hunter2hunter2".into()],
            next_step: Some("ship".into()),
            task_id: Some("T-42".into()),
            branch: Some("main".into()),
            commit: Some("9f3a1c2".into()),
        };
        let out = redact_turn_record(&rec);
        let blob = serde_json::to_string(&out).unwrap();
        assert!(!blob.contains("AKIAIOSFODNN7EXAMPLE"), "{blob}");
        assert!(!blob.contains("ghp_abcdef"), "{blob}");
        assert!(!blob.contains("hunter2hunter2"), "{blob}");
        // Innocuous fields survive intact.
        assert_eq!(out.files_changed, vec!["src/ok.rs".to_string()]);
        assert_eq!(out.commit.as_deref(), Some("9f3a1c2"));
    }

    #[test]
    fn promote_records_are_redacted_and_keep_op_id() {
        let op = PromoteOp {
            op_id: "cm-fixed".into(),
            kind: crate::types::PromoteKind::Decision,
            record: PromoteRecord {
                title: "Adopt sk-abcdefghijklmnopqrstuvwxyz0123".into(),
                summary: "ok".into(),
                evidence: Evidence {
                    test_result: Some("token=abcdef123456".into()),
                    ..Evidence::default()
                },
                ..PromoteRecord::default()
            },
            supersedes: Some("prev-id".into()),
        };
        let out = redact_promote_op(&op);
        assert_eq!(out.op_id, "cm-fixed");
        assert_eq!(out.supersedes.as_deref(), Some("prev-id"));
        assert!(!out.record.title.contains("sk-abcdef"));
        assert!(
            !out.record
                .evidence
                .test_result
                .as_deref()
                .unwrap_or_default()
                .contains("abcdef123456")
        );
    }
}
