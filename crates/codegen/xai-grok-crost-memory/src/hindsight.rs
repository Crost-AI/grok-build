//! The production provider: a direct driver for a Hindsight deployment.
//!
//! Bank names are derived HERE and nowhere else. Nothing above this module
//! knows that `crost--{slug}--grok-private` is a string that exists.
//!
//! Private-bank isolation is by construction, not by server enforcement: all
//! agents share one API key, and each client only ever derives its own private
//! bank name.

use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::config::CrostMemoryConfig;
use crate::config::SecretToken;
use crate::identity::ProjectIdentity;
use crate::provider::MemoryError;
use crate::provider::MemoryProvider;
use crate::types::PromoteKind;
use crate::types::PromoteOp;
use crate::types::PromoteRecord;
use crate::types::ProviderStatus;
use crate::types::RecallItem;
use crate::types::RecallScope;
use crate::types::RetainOp;
use crate::types::TurnRecord;

/// Provider label used in status output and tracing.
pub const PROVIDER_NAME: &str = "hindsight";

/// Recall queries are truncated to this many characters before being sent.
pub const MAX_QUERY_CHARS: usize = 2000;

/// The two bank names for one project/agent pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankNames {
    pub shared: String,
    pub private: String,
}

/// `bankPrefix` if set, else `crost--{slug}`.
pub fn bank_prefix(identity: &ProjectIdentity) -> String {
    identity
        .bank_prefix
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map_or_else(
            || format!("crost--{}", identity.slug),
            std::string::ToString::to_string,
        )
}

/// `{prefix}--shared` and `{prefix}--{agent}-private`.
pub fn bank_names(identity: &ProjectIdentity, agent_id: &str) -> BankNames {
    let prefix = bank_prefix(identity);
    BankNames {
        shared: format!("{prefix}--shared"),
        private: format!("{prefix}--{agent_id}-private"),
    }
}

/// Strip trailing slashes so URL joining is unambiguous.
fn normalize_base(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// `POST` target for recall.
pub fn recall_url(base_url: &str, bank: &str) -> String {
    format!(
        "{}/v1/default/banks/{bank}/memories/recall",
        normalize_base(base_url)
    )
}

/// `POST` target for writes (retain and promote share it).
pub fn memories_url(base_url: &str, bank: &str) -> String {
    format!(
        "{}/v1/default/banks/{bank}/memories",
        normalize_base(base_url)
    )
}

/// `GET` target for the health probe.
pub fn version_url(base_url: &str) -> String {
    format!("{}/version", normalize_base(base_url))
}

/// Truncate on a char boundary so multi-byte queries never panic or split.
pub fn truncate_query(query: &str, max_chars: usize) -> &str {
    match query.char_indices().nth(max_chars) {
        Some((idx, _)) => &query[..idx],
        None => query,
    }
}

/// Recall request body.
pub fn recall_body(query: &str, max_tokens: usize) -> Value {
    json!({
        "query": truncate_query(query, MAX_QUERY_CHARS),
        "max_tokens": max_tokens,
    })
}

fn insert_if_set(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(v) = value
        && !v.trim().is_empty()
    {
        map.insert(key.to_string(), Value::String(v.to_string()));
    }
}

/// Compact, deterministic markdown for a turn record.
///
/// Deterministic matters: the same logical turn re-rendered on retry must
/// produce identical bytes, so a server-side upsert on `document_id` is a true
/// no-op rather than a content change.
pub fn render_turn_record(rec: &TurnRecord) -> String {
    let mut out = String::from("# Turn summary\n");
    if let Some(obj) = non_blank(rec.objective.as_deref()) {
        out.push_str(&format!("Objective: {obj}\n"));
    }
    push_list(&mut out, "Decisions", &rec.decisions);
    push_list(&mut out, "Files changed", &rec.files_changed);
    if !rec.tests.is_empty() {
        out.push_str("Tests:\n");
        for t in &rec.tests {
            out.push_str(&format!("- `{}` => {}\n", t.cmd.trim(), t.result.trim()));
        }
    }
    push_list(&mut out, "Blockers", &rec.blockers);
    if let Some(v) = non_blank(rec.next_step.as_deref()) {
        out.push_str(&format!("Next step: {v}\n"));
    }
    if let Some(v) = non_blank(rec.task_id.as_deref()) {
        out.push_str(&format!("Task: {v}\n"));
    }
    if let Some(v) = non_blank(rec.branch.as_deref()) {
        out.push_str(&format!("Branch: {v}\n"));
    }
    if let Some(v) = non_blank(rec.commit.as_deref()) {
        out.push_str(&format!("Commit: {v}\n"));
    }
    out
}

/// Compact, deterministic markdown for a promotion record.
pub fn render_promote_record(kind: PromoteKind, rec: &PromoteRecord) -> String {
    let mut out = format!("# {kind}: {}\n", rec.title.trim());
    if !rec.summary.trim().is_empty() {
        out.push_str(&format!("{}\n", rec.summary.trim()));
    }
    if let Some(v) = non_blank(rec.status.as_deref()) {
        out.push_str(&format!("Status: {v}\n"));
    }
    push_list(&mut out, "Decisions", &rec.decisions);
    push_list(&mut out, "Files", &rec.files);
    if !rec.evidence.is_empty() {
        out.push_str("Evidence:\n");
        if let Some(v) = non_blank(rec.evidence.commit.as_deref()) {
            out.push_str(&format!("- commit: {v}\n"));
        }
        if let Some(v) = non_blank(rec.evidence.test_cmd.as_deref()) {
            let result = non_blank(rec.evidence.test_result.as_deref()).unwrap_or("(no result)");
            out.push_str(&format!("- test: `{v}` => {result}\n"));
        } else if let Some(v) = non_blank(rec.evidence.test_result.as_deref()) {
            out.push_str(&format!("- test result: {v}\n"));
        }
        if let Some(v) = non_blank(rec.evidence.pr.as_deref()) {
            out.push_str(&format!("- pr: {v}\n"));
        }
    }
    if let Some(v) = non_blank(rec.next_owner.as_deref()) {
        out.push_str(&format!("Next owner: {v}\n"));
    }
    if let Some(v) = non_blank(rec.next_action.as_deref()) {
        out.push_str(&format!("Next action: {v}\n"));
    }
    if let Some(v) = non_blank(rec.task_id.as_deref()) {
        out.push_str(&format!("Task: {v}\n"));
    }
    out
}

fn non_blank(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

fn push_list(out: &mut String, heading: &str, items: &[String]) {
    let items: Vec<&str> = items
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("{heading}:\n"));
    for i in items {
        out.push_str(&format!("- {i}\n"));
    }
}

/// Retain request body — one item, upserted on `document_id = op_id`.
pub fn retain_body(op: &RetainOp, agent_id: &str) -> Value {
    let mut metadata = Map::new();
    metadata.insert("agent".into(), Value::String(agent_id.to_string()));
    metadata.insert(
        "record_kind".into(),
        Value::String("turn_summary".to_string()),
    );
    insert_if_set(&mut metadata, "task_id", op.record.task_id.as_deref());
    insert_if_set(&mut metadata, "branch", op.record.branch.as_deref());
    insert_if_set(&mut metadata, "commit", op.record.commit.as_deref());

    json!({
        "items": [{
            "content": render_turn_record(&op.record),
            "document_id": op.op_id,
            "metadata": Value::Object(metadata),
            "tags": [format!("agent:{agent_id}")],
            "update_mode": "replace",
        }],
        "async": true,
        "operation_id": op.op_id,
    })
}

/// Promote request body — same shape, shared bank, kind-tagged.
pub fn promote_body(op: &PromoteOp, agent_id: &str) -> Value {
    let mut metadata = Map::new();
    metadata.insert("agent".into(), Value::String(agent_id.to_string()));
    metadata.insert("record_kind".into(), Value::String("promotion".to_string()));
    metadata.insert("kind".into(), Value::String(op.kind.to_string()));
    insert_if_set(&mut metadata, "status", op.record.status.as_deref());
    insert_if_set(&mut metadata, "next_owner", op.record.next_owner.as_deref());
    insert_if_set(
        &mut metadata,
        "next_action",
        op.record.next_action.as_deref(),
    );
    insert_if_set(&mut metadata, "task_id", op.record.task_id.as_deref());
    insert_if_set(
        &mut metadata,
        "commit",
        op.record.evidence.commit.as_deref(),
    );
    insert_if_set(
        &mut metadata,
        "test_cmd",
        op.record.evidence.test_cmd.as_deref(),
    );
    insert_if_set(
        &mut metadata,
        "test_result",
        op.record.evidence.test_result.as_deref(),
    );
    insert_if_set(&mut metadata, "pr", op.record.evidence.pr.as_deref());
    insert_if_set(&mut metadata, "supersedes", op.supersedes.as_deref());

    json!({
        "items": [{
            "content": render_promote_record(op.kind, &op.record),
            "document_id": op.op_id,
            "metadata": Value::Object(metadata),
            "tags": [format!("kind:{}", op.kind), format!("agent:{agent_id}")],
            "update_mode": "replace",
        }],
        "async": true,
        "operation_id": op.op_id,
    })
}

/// Map an HTTP status onto the error taxonomy. `None` means success.
///
/// The body is used only for a short diagnostic detail; it is truncated so a
/// verbose server error page cannot flood the logs.
pub fn classify_status(status: u16, body: &str) -> Option<MemoryError> {
    if (200..300).contains(&status) {
        return None;
    }
    let detail = format!("HTTP {status}: {}", truncate_query(body.trim(), 200));
    match status {
        401 | 403 => Some(MemoryError::Auth(detail)),
        s if (400..500).contains(&s) => Some(MemoryError::Invalid(detail)),
        _ => Some(MemoryError::Unavailable(detail)),
    }
}

/// Parse a recall response into items, capped at `max_items`.
///
/// Tolerant by design: a result missing `id` or `text` is skipped rather than
/// failing the whole recall, because a partially-understood response is still
/// worth more than nothing on a fail-open path.
pub fn parse_recall_items(body: &Value, max_items: usize) -> Vec<RecallItem> {
    let Some(results) = body.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|r| {
            let id = r.get("id").and_then(Value::as_str)?;
            let content = r.get("text").and_then(Value::as_str)?;
            if content.trim().is_empty() {
                return None;
            }
            let metadata = r.get("metadata");
            let meta_str = |key: &str| {
                metadata
                    .and_then(|m| m.get(key))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            Some(RecallItem {
                id: id.to_string(),
                content: content.to_string(),
                kind: r.get("type").and_then(Value::as_str).map(str::to_string),
                created_at: r
                    .get("mentioned_at")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                source_agent: meta_str("source_agent"),
                task_id: meta_str("task_id"),
                score: r
                    .get("scores")
                    .and_then(|s| s.get("final"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
            })
        })
        .take(max_items)
        .collect()
}

/// Direct Hindsight driver.
#[derive(Debug)]
pub struct HindsightProvider {
    client: reqwest::Client,
    base_url: String,
    token: Option<SecretToken>,
    banks: BankNames,
    agent_id: String,
}

impl HindsightProvider {
    /// Build a provider for one project. Fails only on configuration the
    /// client can prove is wrong before any network call.
    pub fn new(cfg: &CrostMemoryConfig, identity: &ProjectIdentity) -> Result<Self, MemoryError> {
        let base_url = cfg
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                MemoryError::Invalid(
                    "crost memory: `base_url` is required for the hindsight provider".to_string(),
                )
            })?;
        let timeout = Duration::from_millis(cfg.recall_timeout_ms.max(1));
        let agent_id = cfg.resolved_agent_id();
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|e| MemoryError::Invalid(format!("crost memory: http client: {e}")))?;
        Ok(Self {
            client,
            base_url: normalize_base(base_url),
            token: cfg.load_token(),
            banks: bank_names(identity, &agent_id),
            agent_id,
        })
    }

    /// The bank a scope maps to.
    fn bank(&self, scope: RecallScope) -> &str {
        match scope {
            RecallScope::Private => &self.banks.private,
            RecallScope::Shared => &self.banks.shared,
        }
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t.expose()),
            None => req,
        }
    }

    /// POST a JSON body and classify the outcome. Cancel-safe: dropping the
    /// returned future aborts the request and leaves no local state behind.
    async fn post_json(&self, url: String, body: &Value) -> Result<Value, MemoryError> {
        let resp = self
            .authed(self.client.post(&url))
            .json(body)
            .send()
            .await
            .map_err(|e| MemoryError::Unavailable(describe_reqwest_error(&e)))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if let Some(err) = classify_status(status, &text) {
            return Err(err);
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

/// Describe a transport error without echoing headers or the token.
fn describe_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "request timed out".to_string()
    } else if e.is_connect() {
        "could not connect".to_string()
    } else {
        // `reqwest::Error`'s Display includes the URL but never headers, and
        // this client never puts credentials in a URL.
        e.to_string()
    }
}

#[async_trait]
impl MemoryProvider for HindsightProvider {
    async fn recall(
        &self,
        scope: RecallScope,
        query: &str,
        max_tokens: usize,
        max_items: usize,
    ) -> Result<Vec<RecallItem>, MemoryError> {
        let bank = self.bank(scope);
        let url = recall_url(&self.base_url, bank);
        let body = recall_body(query, max_tokens);
        let value = self.post_json(url, &body).await?;
        let items = parse_recall_items(&value, max_items);
        tracing::debug!(
            scope = scope.as_str(),
            items = items.len(),
            "crost memory recall"
        );
        Ok(items)
    }

    async fn retain_private(&self, op: &RetainOp) -> Result<(), MemoryError> {
        let url = memories_url(&self.base_url, &self.banks.private);
        let body = retain_body(op, &self.agent_id);
        self.post_json(url, &body).await?;
        tracing::debug!(op_id = %op.op_id, "crost memory retained");
        Ok(())
    }

    async fn promote_shared(&self, op: &PromoteOp) -> Result<String, MemoryError> {
        let url = memories_url(&self.base_url, &self.banks.shared);
        let body = promote_body(op, &self.agent_id);
        self.post_json(url, &body).await?;
        tracing::debug!(op_id = %op.op_id, kind = %op.kind, "crost memory promoted");
        Ok(op.op_id.clone())
    }

    async fn status(&self) -> ProviderStatus {
        let url = version_url(&self.base_url);
        let started = Instant::now();
        let outcome = self.authed(self.client.get(&url)).send().await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Ok(resp) if resp.status().is_success() => ProviderStatus {
                healthy: true,
                provider: PROVIDER_NAME,
                endpoint: Some(self.base_url.clone()),
                latency_ms: Some(latency_ms),
                detail: None,
            },
            Ok(resp) => ProviderStatus {
                healthy: false,
                provider: PROVIDER_NAME,
                endpoint: Some(self.base_url.clone()),
                latency_ms: Some(latency_ms),
                detail: Some(format!("HTTP {}", resp.status().as_u16())),
            },
            Err(e) => ProviderStatus {
                healthy: false,
                provider: PROVIDER_NAME,
                endpoint: Some(self.base_url.clone()),
                latency_ms: Some(latency_ms),
                detail: Some(describe_reqwest_error(&e)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Evidence;
    use crate::types::TestEvidence;

    fn identity() -> ProjectIdentity {
        ProjectIdentity {
            project_id: "p1".into(),
            slug: "ohm-storefront".into(),
            bank_prefix: None,
        }
    }

    #[test]
    fn default_bank_names_derive_from_the_slug() {
        let banks = bank_names(&identity(), "grok");
        assert_eq!(banks.shared, "crost--ohm-storefront--shared");
        assert_eq!(banks.private, "crost--ohm-storefront--grok-private");
    }

    #[test]
    fn bank_prefix_override_wins() {
        let id = ProjectIdentity {
            bank_prefix: Some("crost--legacy".into()),
            ..identity()
        };
        let banks = bank_names(&id, "codex");
        assert_eq!(banks.shared, "crost--legacy--shared");
        assert_eq!(banks.private, "crost--legacy--codex-private");
    }

    #[test]
    fn blank_bank_prefix_falls_back_to_the_slug() {
        let id = ProjectIdentity {
            bank_prefix: Some("   ".into()),
            ..identity()
        };
        assert_eq!(bank_prefix(&id), "crost--ohm-storefront");
    }

    #[test]
    fn agents_derive_distinct_private_banks() {
        let grok = bank_names(&identity(), "grok");
        let codex = bank_names(&identity(), "codex");
        assert_ne!(grok.private, codex.private);
        assert_eq!(grok.shared, codex.shared);
    }

    #[test]
    fn urls_are_built_from_the_contract_paths() {
        assert_eq!(
            recall_url("http://h:8080/", "b1"),
            "http://h:8080/v1/default/banks/b1/memories/recall"
        );
        assert_eq!(
            memories_url("http://h:8080", "b1"),
            "http://h:8080/v1/default/banks/b1/memories"
        );
        assert_eq!(version_url("http://h:8080///"), "http://h:8080/version");
    }

    #[test]
    fn query_truncation_respects_char_boundaries() {
        let s = "ünïcödé".repeat(1000);
        let t = truncate_query(&s, MAX_QUERY_CHARS);
        assert_eq!(t.chars().count(), MAX_QUERY_CHARS);
        assert!(s.starts_with(t));
        assert_eq!(truncate_query("short", MAX_QUERY_CHARS), "short");
    }

    #[test]
    fn recall_body_carries_query_and_budget() {
        let body = recall_body("why did we pick sqlite", 800);
        assert_eq!(body["query"], "why did we pick sqlite");
        assert_eq!(body["max_tokens"], 800);
    }

    #[test]
    fn recall_body_truncates_long_queries() {
        let q = "x".repeat(5000);
        let body = recall_body(&q, 800);
        assert_eq!(
            body["query"].as_str().unwrap_or_default().len(),
            MAX_QUERY_CHARS
        );
    }

    fn sample_turn() -> TurnRecord {
        TurnRecord {
            objective: Some("add outbox".into()),
            decisions: vec!["one file per op".into(), "".into()],
            files_changed: vec!["src/outbox.rs".into()],
            tests: vec![TestEvidence {
                cmd: "cargo test".into(),
                result: "42 passed".into(),
            }],
            blockers: vec![],
            next_step: Some("wire the shell".into()),
            task_id: Some("T-42".into()),
            branch: Some("claude/crost-memory".into()),
            commit: Some("9f3a1c2".into()),
        }
    }

    #[test]
    fn turn_record_renders_deterministic_markdown() {
        let rendered = render_turn_record(&sample_turn());
        assert_eq!(rendered, render_turn_record(&sample_turn()));
        let expected = "# Turn summary\n\
             Objective: add outbox\n\
             Decisions:\n- one file per op\n\
             Files changed:\n- src/outbox.rs\n\
             Tests:\n- `cargo test` => 42 passed\n\
             Next step: wire the shell\n\
             Task: T-42\n\
             Branch: claude/crost-memory\n\
             Commit: 9f3a1c2\n";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn empty_turn_record_renders_only_the_heading() {
        assert_eq!(
            render_turn_record(&TurnRecord::default()),
            "# Turn summary\n"
        );
    }

    #[test]
    fn retain_body_matches_the_contract_shape() {
        let op = RetainOp {
            op_id: "cm-abc".into(),
            record: sample_turn(),
        };
        let body = retain_body(&op, "grok");
        assert_eq!(body["async"], true);
        assert_eq!(body["operation_id"], "cm-abc");
        let item = &body["items"][0];
        assert_eq!(item["document_id"], "cm-abc");
        assert_eq!(item["update_mode"], "replace");
        assert_eq!(item["tags"][0], "agent:grok");
        assert_eq!(item["metadata"]["record_kind"], "turn_summary");
        assert_eq!(item["metadata"]["agent"], "grok");
        assert_eq!(item["metadata"]["task_id"], "T-42");
        assert_eq!(item["metadata"]["commit"], "9f3a1c2");
        assert!(
            item["content"]
                .as_str()
                .unwrap_or_default()
                .contains("Objective: add outbox")
        );
    }

    #[test]
    fn retain_metadata_omits_absent_fields() {
        let op = RetainOp {
            op_id: "cm-abc".into(),
            record: TurnRecord {
                objective: Some("x".into()),
                ..TurnRecord::default()
            },
        };
        let body = retain_body(&op, "grok");
        let meta = &body["items"][0]["metadata"];
        assert!(meta.get("task_id").is_none());
        assert!(meta.get("branch").is_none());
        assert!(meta.get("commit").is_none());
    }

    #[test]
    fn promote_body_tags_kind_and_agent() {
        let op = PromoteOp {
            op_id: "cm-xyz".into(),
            kind: PromoteKind::Decision,
            record: PromoteRecord {
                title: "Use sqlite".into(),
                summary: "Local first".into(),
                status: Some("accepted".into()),
                evidence: Evidence {
                    commit: Some("9f3a1c2".into()),
                    test_cmd: Some("cargo test".into()),
                    test_result: Some("ok".into()),
                    pr: Some("#412".into()),
                },
                ..PromoteRecord::default()
            },
            supersedes: Some("cm-old".into()),
        };
        let body = promote_body(&op, "grok");
        let item = &body["items"][0];
        assert_eq!(item["tags"][0], "kind:decision");
        assert_eq!(item["tags"][1], "agent:grok");
        assert_eq!(item["metadata"]["supersedes"], "cm-old");
        assert_eq!(item["metadata"]["status"], "accepted");
        assert_eq!(item["metadata"]["pr"], "#412");
        assert_eq!(item["metadata"]["record_kind"], "promotion");
        let content = item["content"].as_str().unwrap_or_default();
        assert!(content.starts_with("# decision: Use sqlite"), "{content}");
        assert!(content.contains("- test: `cargo test` => ok"), "{content}");
    }

    #[test]
    fn promote_render_skips_an_empty_evidence_block() {
        let rendered = render_promote_record(
            PromoteKind::Blocker,
            &PromoteRecord {
                title: "Blocked".into(),
                summary: "waiting".into(),
                ..PromoteRecord::default()
            },
        );
        assert!(!rendered.contains("Evidence"), "{rendered}");
    }

    #[test]
    fn status_classification_follows_the_taxonomy() {
        assert!(classify_status(200, "").is_none());
        assert!(classify_status(204, "").is_none());
        assert!(matches!(
            classify_status(401, "nope"),
            Some(MemoryError::Auth(_))
        ));
        assert!(matches!(
            classify_status(403, "nope"),
            Some(MemoryError::Auth(_))
        ));
        assert!(matches!(
            classify_status(400, "bad"),
            Some(MemoryError::Invalid(_))
        ));
        assert!(matches!(
            classify_status(404, "gone"),
            Some(MemoryError::Invalid(_))
        ));
        assert!(matches!(
            classify_status(500, "boom"),
            Some(MemoryError::Unavailable(_))
        ));
        assert!(matches!(
            classify_status(503, "boom"),
            Some(MemoryError::Unavailable(_))
        ));
    }

    #[test]
    fn error_detail_is_truncated() {
        let body = "e".repeat(5000);
        let err = classify_status(500, &body).unwrap();
        assert!(err.to_string().len() < 400, "{}", err.to_string().len());
    }

    #[test]
    fn recall_response_maps_onto_recall_items() {
        let body = json!({
            "results": [
                {
                    "id": "m1",
                    "text": "we chose sqlite",
                    "type": "decision",
                    "mentioned_at": "2026-07-12T09:00:00Z",
                    "metadata": { "source_agent": "codex", "task_id": "T-42" },
                    "scores": { "final": 0.91 }
                },
                {
                    "id": "m2",
                    "text": "outbox is bounded",
                    "scores": { "final": 0.4 }
                },
                {
                    "id": "m3",
                    "text": "no scores at all"
                }
            ]
        });
        let items = parse_recall_items(&body, 8);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "m1");
        assert_eq!(items[0].content, "we chose sqlite");
        assert_eq!(items[0].kind.as_deref(), Some("decision"));
        assert_eq!(items[0].created_at.as_deref(), Some("2026-07-12T09:00:00Z"));
        assert_eq!(items[0].source_agent.as_deref(), Some("codex"));
        assert_eq!(items[0].task_id.as_deref(), Some("T-42"));
        assert!((items[0].score - 0.91).abs() < f64::EPSILON);
        assert_eq!(items[1].kind, None);
        assert!((items[1].score - 0.4).abs() < f64::EPSILON);
        // A result with no `scores` object scores zero rather than vanishing.
        assert!(items[2].score.abs() < f64::EPSILON);
        assert_eq!(items[2].source_agent, None);
    }

    #[test]
    fn recall_parsing_is_tolerant_and_capped() {
        let body = json!({
            "results": [
                { "text": "no id" },
                { "id": "m2" },
                { "id": "m3", "text": "   " },
                { "id": "m4", "text": "keep" },
                { "id": "m5", "text": "keep too" }
            ]
        });
        let items = parse_recall_items(&body, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "m4");
    }

    #[test]
    fn missing_results_key_yields_nothing() {
        assert!(parse_recall_items(&json!({}), 8).is_empty());
        assert!(parse_recall_items(&Value::Null, 8).is_empty());
    }

    #[test]
    fn provider_debug_never_prints_the_token() {
        let cfg = CrostMemoryConfig {
            base_url: Some("http://localhost:8080".into()),
            ..CrostMemoryConfig::default()
        };
        let mut p = HindsightProvider::new(&cfg, &identity()).unwrap();
        p.token = Some(SecretToken::new("hs-live-supersecret"));
        let rendered = format!("{p:?}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(rendered.contains("***"), "{rendered}");
    }
}
