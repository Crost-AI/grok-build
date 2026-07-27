//! The `crost_memory_promote_shared` native tool: schema and input parsing.
//!
//! Promotion to the shared bank is NEVER automatic. This module is the seam
//! the shell's tool wrapper calls: it owns the schema the model sees and the
//! validation of what the model sends back, so the wrapper stays a thin
//! adapter. Subagents inherit the parent's private scope and are not offered
//! this tool.

use serde_json::Value;
use serde_json::json;

use crate::types::Evidence;
use crate::types::PromoteKind;
use crate::types::PromoteRecord;

/// Tool name exposed to the model.
pub const PROMOTE_TOOL_NAME: &str = "crost_memory_promote_shared";

/// Human-readable tool description for the shell's tool registration.
pub const PROMOTE_TOOL_DESCRIPTION: &str = "Promote a durable fact to the project's SHARED memory bank, \
visible to every agent working on this project. Use for decisions, verified results, blockers, and \
handoffs that a teammate arriving later would need. Decisions and results should carry evidence \
(a commit, a test command with its result, or a PR). Do not use for routine progress narration.";

/// JSON schema for the tool's input object.
pub fn promote_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "enum": PromoteKind::ALL.map(PromoteKind::as_str),
                "description": "What sort of record this is."
            },
            "title": {
                "type": "string",
                "description": "One-line headline, e.g. 'Use SQLite for the local cache'."
            },
            "summary": {
                "type": "string",
                "description": "A few sentences a teammate could act on without further context."
            },
            "status": {
                "type": "string",
                "description": "Optional state, e.g. 'accepted', 'in progress', 'blocked'."
            },
            "decisions": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Specific choices made."
            },
            "files": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Repo-relative paths this record concerns."
            },
            "evidence": {
                "type": "object",
                "properties": {
                    "commit": { "type": "string" },
                    "test_cmd": { "type": "string" },
                    "test_result": { "type": "string" },
                    "pr": { "type": "string" }
                },
                "additionalProperties": false,
                "description": "Proof. Strongly preferred for kind=decision and kind=result."
            },
            "next_owner": {
                "type": "string",
                "description": "Agent or person who should pick this up."
            },
            "next_action": {
                "type": "string",
                "description": "The single next step."
            },
            "task_id": {
                "type": "string",
                "description": "Task/ticket id, when one exists."
            },
            "supersedes": {
                "type": "string",
                "description": "Id of a shared record this one corrects. The old record is preserved, not overwritten."
            }
        },
        "required": ["kind", "title", "summary"],
        "additionalProperties": false
    })
}

/// Validate and convert a tool-call input object.
///
/// Errors are written for the model to read and retry against, so they name
/// the offending field and what was expected.
pub fn parse_promote_input(
    input: Value,
) -> Result<(PromoteKind, PromoteRecord, Option<String>), String> {
    let Value::Object(obj) = input else {
        return Err(format!(
            "{PROMOTE_TOOL_NAME}: input must be a JSON object with at least `kind`, `title` and `summary`"
        ));
    };

    let kind_raw = require_string(&obj, "kind")?;
    let kind = PromoteKind::parse(kind_raw.trim()).ok_or_else(|| {
        format!(
            "{PROMOTE_TOOL_NAME}: `kind` must be one of decision, result, blocker, handoff (got `{}`)",
            kind_raw.trim()
        )
    })?;

    let title = require_string(&obj, "title")?;
    let summary = require_string(&obj, "summary")?;

    let record = PromoteRecord {
        title,
        summary,
        status: optional_string(&obj, "status")?,
        decisions: optional_string_array(&obj, "decisions")?,
        files: optional_string_array(&obj, "files")?,
        evidence: parse_evidence(obj.get("evidence"))?,
        next_owner: optional_string(&obj, "next_owner")?,
        next_action: optional_string(&obj, "next_action")?,
        task_id: optional_string(&obj, "task_id")?,
    };
    let supersedes = optional_string(&obj, "supersedes")?;
    Ok((kind, record, supersedes))
}

type Obj = serde_json::Map<String, Value>;

fn require_string(obj: &Obj, field: &str) -> Result<String, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Err(format!("{PROMOTE_TOOL_NAME}: `{field}` is required")),
        Some(Value::String(s)) if s.trim().is_empty() => {
            Err(format!("{PROMOTE_TOOL_NAME}: `{field}` must not be empty"))
        }
        Some(Value::String(s)) => Ok(s.trim().to_string()),
        Some(other) => Err(format!(
            "{PROMOTE_TOOL_NAME}: `{field}` must be a string (got {})",
            type_name(other)
        )),
    }
}

fn optional_string(obj: &Obj, field: &str) -> Result<Option<String>, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(other) => Err(format!(
            "{PROMOTE_TOOL_NAME}: `{field}` must be a string (got {})",
            type_name(other)
        )),
    }
}

fn optional_string_array(obj: &Obj, field: &str) -> Result<Vec<String>, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .filter_map(|(i, v)| match v {
                Value::String(s) if s.trim().is_empty() => None,
                Value::String(s) => Some(Ok(s.trim().to_string())),
                other => Some(Err(format!(
                    "{PROMOTE_TOOL_NAME}: `{field}[{i}]` must be a string (got {})",
                    type_name(other)
                ))),
            })
            .collect(),
        Some(other) => Err(format!(
            "{PROMOTE_TOOL_NAME}: `{field}` must be an array of strings (got {})",
            type_name(other)
        )),
    }
}

fn parse_evidence(value: Option<&Value>) -> Result<Evidence, String> {
    match value {
        None | Some(Value::Null) => Ok(Evidence::default()),
        Some(Value::Object(obj)) => Ok(Evidence {
            commit: optional_string(obj, "commit")?,
            test_cmd: optional_string(obj, "test_cmd")?,
            test_result: optional_string(obj, "test_result")?,
            pr: optional_string(obj, "pr")?,
        }),
        Some(other) => Err(format!(
            "{PROMOTE_TOOL_NAME}: `evidence` must be an object with optional commit, test_cmd, test_result, pr (got {})",
            type_name(other)
        )),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_declares_the_required_fields() {
        let schema = promote_input_schema();
        assert_eq!(schema["type"], "object");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(required, vec!["kind", "title", "summary"]);
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn schema_enumerates_every_kind() {
        let schema = promote_input_schema();
        let kinds: Vec<&str> = schema["properties"]["kind"]["enum"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(kinds, vec!["decision", "result", "blocker", "handoff"]);
    }

    #[test]
    fn schema_covers_every_optional_field() {
        let schema = promote_input_schema();
        let props = &schema["properties"];
        for field in [
            "kind",
            "title",
            "summary",
            "status",
            "decisions",
            "files",
            "evidence",
            "next_owner",
            "next_action",
            "task_id",
            "supersedes",
        ] {
            assert!(!props[field].is_null(), "schema is missing `{field}`");
        }
        let ev = &props["evidence"]["properties"];
        for field in ["commit", "test_cmd", "test_result", "pr"] {
            assert!(!ev[field].is_null(), "evidence schema is missing `{field}`");
        }
    }

    #[test]
    fn minimal_input_parses() {
        let (kind, rec, supersedes) = parse_promote_input(json!({
            "kind": "handoff",
            "title": " Hand over the outbox ",
            "summary": "Flusher is wired; the shell still needs to call it."
        }))
        .unwrap();
        assert_eq!(kind, PromoteKind::Handoff);
        assert_eq!(rec.title, "Hand over the outbox");
        assert!(rec.decisions.is_empty());
        assert!(rec.evidence.is_empty());
        assert_eq!(supersedes, None);
    }

    #[test]
    fn full_input_parses() {
        let (kind, rec, supersedes) = parse_promote_input(json!({
            "kind": "decision",
            "title": "Use SQLite",
            "summary": "Local-first cache.",
            "status": "accepted",
            "decisions": ["sqlite over postgres", "", "bundled build"],
            "files": ["crates/codegen/x/src/lib.rs"],
            "evidence": {
                "commit": "9f3a1c2",
                "test_cmd": "cargo test -p x",
                "test_result": "42 passed",
                "pr": "#412"
            },
            "next_owner": "codex",
            "next_action": "wire the shell",
            "task_id": "T-42",
            "supersedes": "cm-old"
        }))
        .unwrap();
        assert_eq!(kind, PromoteKind::Decision);
        assert_eq!(rec.status.as_deref(), Some("accepted"));
        assert_eq!(rec.decisions, vec!["sqlite over postgres", "bundled build"]);
        assert_eq!(rec.files.len(), 1);
        assert_eq!(rec.evidence.commit.as_deref(), Some("9f3a1c2"));
        assert_eq!(rec.evidence.pr.as_deref(), Some("#412"));
        assert_eq!(rec.next_owner.as_deref(), Some("codex"));
        assert_eq!(rec.task_id.as_deref(), Some("T-42"));
        assert_eq!(supersedes.as_deref(), Some("cm-old"));
    }

    #[test]
    fn non_object_input_is_rejected() {
        let err = parse_promote_input(json!("just a string")).unwrap_err();
        assert!(err.contains("must be a JSON object"), "{err}");
    }

    #[test]
    fn missing_required_fields_name_themselves() {
        let err = parse_promote_input(json!({ "title": "t", "summary": "s" })).unwrap_err();
        assert!(err.contains("`kind` is required"), "{err}");
        let err = parse_promote_input(json!({ "kind": "result", "summary": "s" })).unwrap_err();
        assert!(err.contains("`title` is required"), "{err}");
        let err = parse_promote_input(json!({ "kind": "result", "title": "t" })).unwrap_err();
        assert!(err.contains("`summary` is required"), "{err}");
    }

    #[test]
    fn blank_required_fields_are_rejected() {
        let err = parse_promote_input(json!({ "kind": "result", "title": "  ", "summary": "s" }))
            .unwrap_err();
        assert!(err.contains("`title` must not be empty"), "{err}");
    }

    #[test]
    fn an_unknown_kind_lists_the_valid_ones() {
        let err = parse_promote_input(json!({ "kind": "musing", "title": "t", "summary": "s" }))
            .unwrap_err();
        assert!(err.contains("decision, result, blocker, handoff"), "{err}");
        assert!(err.contains("musing"), "{err}");
    }

    #[test]
    fn wrong_types_are_reported_with_the_type_seen() {
        let err =
            parse_promote_input(json!({ "kind": 7, "title": "t", "summary": "s" })).unwrap_err();
        assert!(
            err.contains("`kind` must be a string (got number)"),
            "{err}"
        );

        let err = parse_promote_input(json!({
            "kind": "result", "title": "t", "summary": "s", "decisions": "not an array"
        }))
        .unwrap_err();
        assert!(
            err.contains("`decisions` must be an array of strings (got string)"),
            "{err}"
        );

        let err = parse_promote_input(json!({
            "kind": "result", "title": "t", "summary": "s", "decisions": ["ok", 3]
        }))
        .unwrap_err();
        assert!(err.contains("`decisions[1]` must be a string"), "{err}");

        let err = parse_promote_input(json!({
            "kind": "result", "title": "t", "summary": "s", "evidence": "9f3a1c2"
        }))
        .unwrap_err();
        assert!(err.contains("`evidence` must be an object"), "{err}");
    }

    #[test]
    fn nulls_are_treated_as_absent() {
        let (_, rec, supersedes) = parse_promote_input(json!({
            "kind": "blocker",
            "title": "t",
            "summary": "s",
            "status": null,
            "files": null,
            "evidence": null,
            "supersedes": null
        }))
        .unwrap();
        assert_eq!(rec.status, None);
        assert!(rec.files.is_empty());
        assert!(rec.evidence.is_empty());
        assert_eq!(supersedes, None);
    }

    #[test]
    fn every_kind_round_trips_through_the_parser() {
        for kind in PromoteKind::ALL {
            let (parsed, _, _) = parse_promote_input(json!({
                "kind": kind.as_str(), "title": "t", "summary": "s"
            }))
            .unwrap();
            assert_eq!(parsed, kind);
        }
    }
}
