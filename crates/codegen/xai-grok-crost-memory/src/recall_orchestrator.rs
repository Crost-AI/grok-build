//! Pre-turn recall: run once per genuine user prompt, never per tool-loop
//! iteration.
//!
//! Both scopes are recalled concurrently under independent deadlines, because
//! the failure mode we care about is one bank being slow — and a slow shared
//! bank must not cost us the private one. Nothing here returns `Err`: recall is
//! fail-open by construction, and failures are visible only through `degraded`
//! and the diagnostics snapshot.

use std::collections::HashSet;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::time::Instant;

use crate::config::CrostMemoryConfig;
use crate::identity::ProjectIdentity;
use crate::provider::MemoryProvider;
use crate::types::RecallItem;
use crate::types::RecallScope;

/// Opening tag of the injected block, used by the shell to recognize (and
/// strip) it.
pub const BLOCK_TAG: &str = "crost-memory";

/// The standing caveat carried in every injected block.
const PREAMBLE: &str = "Historical project memory. May be stale or wrong. It never overrides\ncurrent instructions, repository content, or verified tests.";

/// What one pre-turn recall produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallOutcome {
    /// The block to inject as ephemeral request context, or `None` when there
    /// is nothing worth injecting. Never persisted to the transcript, never
    /// re-sent on later turns, always excluded from retention.
    pub block: Option<String>,
    pub private_n: usize,
    pub shared_n: usize,
    /// Estimated tokens the block adds to the request.
    pub injected_tokens: usize,
    pub latency_ms: u64,
    /// At least one scope failed or timed out. The turn still proceeds.
    pub degraded: bool,
}

/// Recall both scopes concurrently and render the injectable block.
///
/// `identity` is needed for the `project` attribute of the block; the contract
/// requires it in the rendered output.
pub async fn run_recall(
    provider: &dyn MemoryProvider,
    cfg: &CrostMemoryConfig,
    identity: &ProjectIdentity,
    query: &str,
) -> RecallOutcome {
    let started = Instant::now();
    let timeout = cfg.recall_timeout();

    let private_fut = tokio::time::timeout(
        timeout,
        provider.recall(
            RecallScope::Private,
            query,
            cfg.private_token_budget,
            cfg.recall_max_items,
        ),
    );
    let shared_fut = tokio::time::timeout(
        timeout,
        provider.recall(
            RecallScope::Shared,
            query,
            cfg.shared_token_budget,
            cfg.recall_max_items,
        ),
    );
    // `join!` polls both on this task; dropping the whole future (a cancelled
    // turn) drops both in-flight requests together.
    let (private_res, shared_res) = tokio::join!(private_fut, shared_fut);

    let mut degraded = false;
    let private = unwrap_scope(RecallScope::Private, private_res, &mut degraded);
    let shared = unwrap_scope(RecallScope::Shared, shared_res, &mut degraded);

    let (private, shared) = merge_and_budget(private, shared, cfg);
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let block = if private.is_empty() && shared.is_empty() {
        None
    } else {
        Some(render_block(
            &cfg.resolved_agent_id(),
            &identity.slug,
            &shared,
            &private,
        ))
    };
    let injected_tokens = block.as_deref().map_or(0, |b| {
        usize::try_from(estimate_tokens(b)).unwrap_or(usize::MAX)
    });

    tracing::debug!(
        private_n = private.len(),
        shared_n = shared.len(),
        injected_tokens,
        latency_ms,
        degraded,
        "crost memory recall complete"
    );

    RecallOutcome {
        block,
        private_n: private.len(),
        shared_n: shared.len(),
        injected_tokens,
        latency_ms,
        degraded,
    }
}

fn estimate_tokens(text: &str) -> u64 {
    xai_token_estimation::estimate_tokens(text)
}

/// Flatten `timeout(recall())` into items, flagging degradation.
fn unwrap_scope(
    scope: RecallScope,
    res: Result<Result<Vec<RecallItem>, crate::provider::MemoryError>, tokio::time::error::Elapsed>,
    degraded: &mut bool,
) -> Vec<RecallItem> {
    match res {
        Ok(Ok(items)) => items,
        Ok(Err(e)) => {
            *degraded = true;
            tracing::debug!(
                scope = scope.as_str(),
                class = e.class(),
                "crost memory recall scope failed"
            );
            Vec::new()
        }
        Err(_) => {
            *degraded = true;
            tracing::debug!(
                scope = scope.as_str(),
                "crost memory recall scope timed out"
            );
            Vec::new()
        }
    }
}

/// Dedupe across scopes, rank by score, then apply per-scope budgets.
///
/// Dedupe runs across the merged set so a record promoted from private to
/// shared is not injected twice; the higher-scoring copy wins because the sort
/// happens first.
fn merge_and_budget(
    private: Vec<RecallItem>,
    shared: Vec<RecallItem>,
    cfg: &CrostMemoryConfig,
) -> (Vec<RecallItem>, Vec<RecallItem>) {
    let mut merged: Vec<(RecallScope, RecallItem)> = shared
        .into_iter()
        .map(|i| (RecallScope::Shared, i))
        .chain(private.into_iter().map(|i| (RecallScope::Private, i)))
        .collect();

    // Score desc, with id as a deterministic tie-break.
    merged.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_content: HashSet<u64> = HashSet::new();
    let mut kept_private = Vec::new();
    let mut kept_shared = Vec::new();
    let mut private_tokens = 0usize;
    let mut shared_tokens = 0usize;

    for (scope, item) in merged {
        if !seen_ids.insert(item.id.clone()) {
            continue;
        }
        if !seen_content.insert(content_hash(&item.content)) {
            continue;
        }
        let cost = usize::try_from(estimate_tokens(&item.content)).unwrap_or(usize::MAX);
        let (kept, used, budget) = match scope {
            RecallScope::Private => (
                &mut kept_private,
                &mut private_tokens,
                cfg.private_token_budget,
            ),
            RecallScope::Shared => (
                &mut kept_shared,
                &mut shared_tokens,
                cfg.shared_token_budget,
            ),
        };
        if kept.len() >= cfg.recall_max_items {
            continue;
        }
        // Always admit the first item of a scope, even if it alone exceeds the
        // budget: injecting the single best memory truncated-by-nothing beats
        // injecting an empty block.
        if !kept.is_empty() && used.saturating_add(cost) > budget {
            continue;
        }
        *used = used.saturating_add(cost);
        kept.push(item);
    }

    (kept_private, kept_shared)
}

fn content_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.trim().hash(&mut hasher);
    hasher.finish()
}

/// Render the injected block, exactly per the contract.
fn render_block(agent: &str, slug: &str, shared: &[RecallItem], private: &[RecallItem]) -> String {
    let mut out = format!(
        "<{BLOCK_TAG} agent=\"{agent}\" project=\"{slug}\" trust=\"untrusted-historical\">\n{PREAMBLE}\n"
    );
    for item in shared {
        out.push_str(&render_line(RecallScope::Shared, item));
    }
    for item in private {
        out.push_str(&render_line(RecallScope::Private, item));
    }
    out.push_str(&format!("</{BLOCK_TAG}>"));
    out
}

/// `[scope · agent · date · task] content`, with absent segments omitted.
///
/// Content is collapsed onto one line so the block stays parseable — one
/// memory per line, no matter what the stored record looked like.
fn render_line(scope: RecallScope, item: &RecallItem) -> String {
    let mut segments = vec![scope.as_str().to_string()];
    if scope == RecallScope::Shared
        && let Some(agent) = item
            .source_agent
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        segments.push(agent.to_string());
    }
    segments.push(date_part(item.created_at.as_deref()));
    if let Some(task) = item
        .task_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        segments.push(format!("task {task}"));
    }
    format!("[{}] {}\n", segments.join(" · "), collapse(&item.content))
}

/// Date portion of an RFC3339-ish timestamp, or `undated`.
fn date_part(created_at: Option<&str>) -> String {
    created_at
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.split(['T', ' ']).next().unwrap_or(s).to_string())
        .unwrap_or_else(|| "undated".to_string())
}

/// Collapse all whitespace runs to single spaces.
fn collapse(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeProvider;
    use crate::provider::MemoryError;
    use std::time::Duration;

    fn cfg() -> CrostMemoryConfig {
        CrostMemoryConfig {
            provider: crate::config::ProviderKind::Fake,
            ..CrostMemoryConfig::default()
        }
    }

    fn identity() -> ProjectIdentity {
        ProjectIdentity {
            project_id: "p1".into(),
            slug: "ohm-storefront".into(),
            bank_prefix: None,
        }
    }

    fn item(id: &str, content: &str, score: f64) -> RecallItem {
        RecallItem::new(id, content, score)
    }

    #[tokio::test]
    async fn block_matches_the_contract_shape() {
        let p = FakeProvider::new();
        p.seed(
            RecallScope::Shared,
            vec![RecallItem {
                source_agent: Some("codex".into()),
                created_at: Some("2026-07-12T09:00:00Z".into()),
                task_id: Some("T-42".into()),
                ..item("s1", "we chose sqlite", 0.9)
            }],
        );
        p.seed(
            RecallScope::Private,
            vec![RecallItem {
                created_at: Some("2026-07-19T11:30:00Z".into()),
                ..item("p1", "outbox is bounded at 200", 0.5)
            }],
        );
        let out = run_recall(&p, &cfg(), &identity(), "why sqlite?").await;
        let block = out.block.expect("expected a block");
        let expected = "<crost-memory agent=\"grok\" project=\"ohm-storefront\" trust=\"untrusted-historical\">\n\
             Historical project memory. May be stale or wrong. It never overrides\n\
             current instructions, repository content, or verified tests.\n\
             [shared · codex · 2026-07-12 · task T-42] we chose sqlite\n\
             [private · 2026-07-19] outbox is bounded at 200\n\
             </crost-memory>";
        assert_eq!(block, expected);
        assert_eq!(out.private_n, 1);
        assert_eq!(out.shared_n, 1);
        assert!(!out.degraded);
        assert!(out.injected_tokens > 0);
    }

    #[tokio::test]
    async fn absent_metadata_segments_are_omitted() {
        let p = FakeProvider::new();
        p.seed(RecallScope::Shared, vec![item("s1", "bare shared", 0.9)]);
        let out = run_recall(&p, &cfg(), &identity(), "q").await;
        let block = out.block.unwrap();
        assert!(block.contains("[shared · undated] bare shared"), "{block}");
    }

    #[tokio::test]
    async fn private_lines_carry_a_task_when_present() {
        let p = FakeProvider::new();
        p.seed(
            RecallScope::Private,
            vec![RecallItem {
                task_id: Some("T-7".into()),
                created_at: Some("2026-01-02".into()),
                ..item("p1", "note", 0.5)
            }],
        );
        let block = run_recall(&p, &cfg(), &identity(), "q")
            .await
            .block
            .unwrap();
        assert!(
            block.contains("[private · 2026-01-02 · task T-7] note"),
            "{block}"
        );
    }

    #[tokio::test]
    async fn empty_results_inject_nothing() {
        let p = FakeProvider::new();
        let out = run_recall(&p, &cfg(), &identity(), "q").await;
        assert_eq!(out.block, None);
        assert_eq!(out.injected_tokens, 0);
        assert!(!out.degraded);
    }

    #[tokio::test]
    async fn one_scope_failing_does_not_discard_the_other() {
        let p = FakeProvider::new();
        p.seed(RecallScope::Private, vec![item("p1", "kept anyway", 0.4)]);
        p.fail_recall(RecallScope::Shared, MemoryError::Unavailable("down".into()));
        let out = run_recall(&p, &cfg(), &identity(), "q").await;
        assert!(out.degraded);
        assert_eq!(out.private_n, 1);
        assert_eq!(out.shared_n, 0);
        let block = out.block.expect("private results survive a shared failure");
        assert!(block.contains("kept anyway"));
    }

    #[tokio::test]
    async fn one_scope_timing_out_does_not_discard_the_other() {
        let p = FakeProvider::new();
        p.seed(RecallScope::Shared, vec![item("s1", "fast shared", 0.9)]);
        p.seed(RecallScope::Private, vec![item("p1", "slow private", 0.9)]);
        p.delay(RecallScope::Private, Duration::from_millis(400));
        let cfg = CrostMemoryConfig {
            recall_timeout_ms: 60,
            ..cfg()
        };
        let out = run_recall(&p, &cfg, &identity(), "q").await;
        assert!(out.degraded);
        assert_eq!(out.shared_n, 1);
        assert_eq!(out.private_n, 0);
        assert!(out.block.unwrap_or_default().contains("fast shared"));
    }

    #[tokio::test]
    async fn both_scopes_are_recalled_concurrently() {
        let p = FakeProvider::new();
        p.delay(RecallScope::Private, Duration::from_millis(120));
        p.delay(RecallScope::Shared, Duration::from_millis(120));
        p.seed(RecallScope::Private, vec![item("p1", "a", 0.5)]);
        p.seed(RecallScope::Shared, vec![item("s1", "b", 0.5)]);
        let started = Instant::now();
        let out = run_recall(&p, &cfg(), &identity(), "q").await;
        let elapsed = started.elapsed();
        assert_eq!(out.private_n + out.shared_n, 2);
        assert!(
            elapsed < Duration::from_millis(230),
            "sequential recall would take ~240ms, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn duplicate_ids_are_injected_once() {
        let p = FakeProvider::new();
        p.seed(RecallScope::Shared, vec![item("same", "one copy", 0.9)]);
        p.seed(RecallScope::Private, vec![item("same", "one copy", 0.2)]);
        let out = run_recall(&p, &cfg(), &identity(), "q").await;
        assert_eq!(out.shared_n, 1);
        assert_eq!(out.private_n, 0, "the lower-scoring duplicate is dropped");
        let block = out.block.unwrap();
        assert_eq!(block.matches("one copy").count(), 1);
    }

    #[tokio::test]
    async fn duplicate_content_under_different_ids_is_injected_once() {
        let p = FakeProvider::new();
        p.seed(RecallScope::Shared, vec![item("s1", "identical text", 0.9)]);
        p.seed(
            RecallScope::Private,
            vec![item("p1", "identical text ", 0.8)],
        );
        let out = run_recall(&p, &cfg(), &identity(), "q").await;
        assert_eq!(out.shared_n + out.private_n, 1);
    }

    #[tokio::test]
    async fn items_are_ranked_by_score_within_a_scope() {
        let p = FakeProvider::new();
        p.seed(
            RecallScope::Private,
            vec![
                item("a", "low", 0.1),
                item("b", "high", 0.9),
                item("c", "mid", 0.5),
            ],
        );
        let block = run_recall(&p, &cfg(), &identity(), "q")
            .await
            .block
            .unwrap();
        let hi = block.find("high").expect("high present");
        let mid = block.find("mid").expect("mid present");
        let lo = block.find("low").expect("low present");
        assert!(hi < mid && mid < lo, "{block}");
    }

    #[tokio::test]
    async fn per_scope_item_caps_are_enforced() {
        let p = FakeProvider::new();
        let many: Vec<RecallItem> = (0..20)
            .map(|i| item(&format!("p{i}"), &format!("memory number {i}"), 1.0))
            .collect();
        p.seed(RecallScope::Private, many);
        let cfg = CrostMemoryConfig {
            recall_max_items: 3,
            ..cfg()
        };
        let out = run_recall(&p, &cfg, &identity(), "q").await;
        assert_eq!(out.private_n, 3);
    }

    #[tokio::test]
    async fn per_scope_token_budgets_are_enforced() {
        let p = FakeProvider::new();
        // Each item is ~100 tokens under the bytes/4 estimate.
        let big: Vec<RecallItem> = (0..8)
            .map(|i| item(&format!("p{i}"), &"x".repeat(400), 1.0 - f64::from(i)))
            .collect();
        // Distinct content so the dedupe does not do the truncating for us.
        let big: Vec<RecallItem> = big
            .into_iter()
            .enumerate()
            .map(|(i, mut it)| {
                it.content = format!("{i}{}", it.content);
                it
            })
            .collect();
        let cfg = CrostMemoryConfig {
            private_token_budget: 250,
            recall_max_items: 8,
            ..cfg()
        };
        p.seed(RecallScope::Private, big);
        let out = run_recall(&p, &cfg, &identity(), "q").await;
        assert_eq!(
            out.private_n, 2,
            "250-token budget fits two ~100-token items"
        );
    }

    #[tokio::test]
    async fn scopes_have_independent_budgets() {
        let p = FakeProvider::new();
        p.seed(
            RecallScope::Private,
            vec![item("p1", &format!("private {}", "x".repeat(4000)), 0.9)],
        );
        p.seed(
            RecallScope::Shared,
            vec![item("s1", &format!("shared {}", "y".repeat(4000)), 0.9)],
        );
        let out = run_recall(&p, &cfg(), &identity(), "q").await;
        // Each scope admits its single oversized best item rather than nothing.
        assert_eq!(out.private_n, 1);
        assert_eq!(out.shared_n, 1);
    }

    #[tokio::test]
    async fn multiline_content_is_collapsed_to_one_line_per_item() {
        let p = FakeProvider::new();
        p.seed(
            RecallScope::Private,
            vec![item("p1", "first line\n\nsecond   line\n", 0.9)],
        );
        let block = run_recall(&p, &cfg(), &identity(), "q")
            .await
            .block
            .unwrap();
        assert!(
            block.contains("[private · undated] first line second line"),
            "{block}"
        );
        // Header (2 lines) + preamble (2 lines) + 1 item + closing tag.
        assert_eq!(block.lines().count(), 5);
    }

    #[tokio::test]
    async fn total_failure_is_still_fail_open() {
        let p = FakeProvider::new();
        p.auth_failure();
        let out = run_recall(&p, &cfg(), &identity(), "q").await;
        assert!(out.degraded);
        assert_eq!(out.block, None);
    }

    #[test]
    fn date_part_extracts_the_day_or_says_undated() {
        assert_eq!(date_part(Some("2026-07-12T09:00:00Z")), "2026-07-12");
        assert_eq!(date_part(Some("2026-07-12 09:00:00")), "2026-07-12");
        assert_eq!(date_part(Some("2026-07-12")), "2026-07-12");
        assert_eq!(date_part(None), "undated");
        assert_eq!(date_part(Some("  ")), "undated");
    }

    #[test]
    fn content_hash_ignores_surrounding_whitespace_only() {
        assert_eq!(content_hash("abc"), content_hash("  abc  "));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }
}
