//! Bounded, persistent write queue.
//!
//! Turn completion never blocks on the network: writes land here first (one
//! small JSON file per op, written to a temp name and renamed into place, so a
//! crash mid-write can never leave a half-parsed entry) and a detached flusher
//! drains them.
//!
//! Retries re-send the file verbatim, `op_id` included, so the server's upsert
//! on `document_id` collapses duplicates. That is the whole reason the op id is
//! generated once, at enqueue, and stored — never regenerated on retry.
//!
//! File IO here is synchronous std::fs on purpose: the operations are a few
//! hundred bytes each and always run on a detached task, so the complexity of
//! async file IO would buy nothing.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

use crate::provider::MemoryError;
use crate::provider::MemoryProvider;
use crate::types::PromoteOp;
use crate::types::RetainOp;

/// Maximum queued ops before the oldest are dropped.
pub const DEFAULT_MAX_OPS: usize = 200;

/// Ceiling on the exponential backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// A queued write, tagged so the flusher knows which provider call to make.
///
/// The tag is `outbox_op`, not `kind`: an internally tagged enum splices its
/// tag into the variant's own field set, and `PromoteOp` already has a `kind`.
/// A collision there would silently produce entries that serialize fine and
/// fail to parse on the way back out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outbox_op", rename_all = "snake_case")]
pub enum OutboxOp {
    Retain(RetainOp),
    Promote(PromoteOp),
}

impl OutboxOp {
    /// The stable idempotency key of the underlying op.
    pub fn op_id(&self) -> &str {
        match self {
            Self::Retain(o) => &o.op_id,
            Self::Promote(o) => &o.op_id,
        }
    }

    /// Short label for tracing.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Retain(_) => "retain",
            Self::Promote(_) => "promote",
        }
    }
}

/// One on-disk queue entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OutboxEntry {
    op: OutboxOp,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    next_attempt_at_ms: u64,
}

/// What one flush pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlushOutcome {
    /// Ops accepted by the provider and removed from the queue.
    pub sent: usize,
    /// Ops the provider rejected as invalid, or entries too corrupt to parse.
    /// Both are removed — retrying them can never succeed.
    pub dropped: usize,
    /// Ops left for a later pass (not yet due, or the pass circuit-broke).
    pub deferred: usize,
    /// The provider rejected our credentials. The caller surfaces exactly ONE
    /// visible warning for this; nothing was dropped.
    pub auth_failed: bool,
    /// The pass stopped early after an `Unavailable` error.
    pub circuit_broken: bool,
    /// Queue depth after the pass.
    pub remaining: usize,
}

/// Backoff for an op that has now failed `attempts` times.
pub fn backoff_delay(attempts: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempts.min(32));
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Filename timestamp, forced strictly increasing within the process.
///
/// Two ops enqueued in the same millisecond would otherwise sort by their
/// random op ids, and "drop the oldest" would drop an arbitrary one. Bumping
/// the stamp keeps the `{millis:016}-{op_id}` name format while making
/// lexicographic order a true arrival order.
fn next_filename_ms() -> u64 {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut prev = LAST.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let candidate = now_ms().max(prev.saturating_add(1));
        match LAST.compare_exchange_weak(
            prev,
            candidate,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return candidate,
            Err(actual) => prev = actual,
        }
    }
}

/// A per-project persistent outbox directory.
#[derive(Debug, Clone)]
pub struct Outbox {
    dir: PathBuf,
    max_ops: usize,
}

impl Outbox {
    /// Use `dir` directly as the queue directory.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_ops: DEFAULT_MAX_OPS,
        }
    }

    /// `root/<project_id>` — the layout each fork uses under its own home
    /// (for this fork, `~/.grok/crost-memory/outbox/`).
    pub fn for_project(root: impl AsRef<Path>, project_id: &str) -> Self {
        Self::new(root.as_ref().join(sanitize_component(project_id)))
    }

    /// Override the queue bound (tests use small values).
    pub fn with_max_ops(mut self, max_ops: usize) -> Self {
        self.max_ops = max_ops.max(1);
        self
    }

    /// The directory this queue lives in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Queue a retain op. Redaction happens here, so secrets never touch disk.
    pub fn enqueue_retain(&self, op: &RetainOp) -> std::io::Result<PathBuf> {
        self.enqueue(&OutboxOp::Retain(op.clone()))
    }

    /// Queue a promote op.
    pub fn enqueue_promote(&self, op: &PromoteOp) -> std::io::Result<PathBuf> {
        self.enqueue(&OutboxOp::Promote(op.clone()))
    }

    /// Queue any op: redact, write temp, rename, then enforce the bound.
    pub fn enqueue(&self, op: &OutboxOp) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        let redacted = crate::redact::redact_outbox_op(op);
        let entry = OutboxEntry {
            op: redacted,
            attempts: 0,
            next_attempt_at_ms: 0,
        };
        let name = format!(
            "{:016}-{}.json",
            next_filename_ms(),
            sanitize_component(op.op_id())
        );
        let final_path = self.dir.join(&name);
        let tmp_path = self.dir.join(format!("{name}.tmp"));
        let body = serde_json::to_vec(&entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp_path, &body)?;
        std::fs::rename(&tmp_path, &final_path)?;
        self.enforce_bound();
        Ok(final_path)
    }

    /// Queued op count.
    pub fn depth(&self) -> usize {
        self.list_files().len()
    }

    /// Age of the oldest queued op, derived from its filename timestamp.
    pub fn oldest_age(&self) -> Option<Duration> {
        let files = self.list_files();
        let oldest = files.first()?;
        let millis = timestamp_of(oldest)?;
        let now = now_ms();
        Some(Duration::from_millis(now.saturating_sub(millis)))
    }

    /// Attempt every due op, oldest first.
    ///
    /// Stops the pass on the first `Unavailable` (the backend is down; hammering
    /// it with the rest of the queue helps nobody) and on the first `Auth`
    /// (the same key will be rejected for every remaining op).
    pub async fn flush(&self, provider: &dyn MemoryProvider) -> FlushOutcome {
        let mut outcome = FlushOutcome::default();
        let now = now_ms();
        for path in self.list_files() {
            let entry = match self.read_entry(&path) {
                Some(e) => e,
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        "crost memory: discarding unreadable outbox entry"
                    );
                    self.remove(&path);
                    outcome.dropped += 1;
                    continue;
                }
            };
            if entry.next_attempt_at_ms > now {
                outcome.deferred += 1;
                continue;
            }
            let result = match &entry.op {
                OutboxOp::Retain(op) => provider.retain_private(op).await,
                OutboxOp::Promote(op) => provider.promote_shared(op).await.map(|_| ()),
            };
            match result {
                Ok(()) => {
                    self.remove(&path);
                    outcome.sent += 1;
                }
                Err(MemoryError::Unavailable(detail)) => {
                    let attempts = entry.attempts.saturating_add(1);
                    let delay = backoff_delay(attempts);
                    let rescheduled = OutboxEntry {
                        op: entry.op,
                        attempts,
                        next_attempt_at_ms: now_ms()
                            .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
                    };
                    self.write_entry(&path, &rescheduled);
                    tracing::debug!(
                        attempts,
                        backoff_ms = delay.as_millis(),
                        detail = %detail,
                        "crost memory: outbox flush deferred"
                    );
                    outcome.deferred += 1;
                    outcome.circuit_broken = true;
                    break;
                }
                Err(MemoryError::Auth(detail)) => {
                    tracing::debug!(detail = %detail, "crost memory: outbox flush rejected");
                    outcome.auth_failed = true;
                    outcome.deferred += 1;
                    break;
                }
                Err(MemoryError::Invalid(detail)) => {
                    tracing::warn!(
                        kind = entry.op.kind(),
                        detail = %detail,
                        "crost memory: dropping outbox op the server rejected"
                    );
                    self.remove(&path);
                    outcome.dropped += 1;
                }
            }
        }
        outcome.remaining = self.depth();
        outcome
    }

    /// Queue files, oldest first (filenames sort chronologically by design).
    fn list_files(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        files
    }

    fn read_entry(&self, path: &Path) -> Option<OutboxEntry> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write_entry(&self, path: &Path, entry: &OutboxEntry) {
        let Ok(body) = serde_json::to_vec(entry) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn remove(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// Drop the OLDEST entries once the queue exceeds its bound. Dropping the
    /// oldest keeps the most recent context, which is the more useful half.
    fn enforce_bound(&self) {
        let files = self.list_files();
        if files.len() <= self.max_ops {
            return;
        }
        let excess = files.len() - self.max_ops;
        for path in files.into_iter().take(excess) {
            tracing::warn!(
                path = %path.display(),
                max_ops = self.max_ops,
                "crost memory: outbox full, dropping oldest op"
            );
            self.remove(&path);
        }
    }
}

/// Millisecond prefix of a queue filename.
fn timestamp_of(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let (millis, _) = name.split_once('-')?;
    millis.parse().ok()
}

/// Keep filenames well-behaved regardless of what a project id contains.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeProvider;
    use crate::types::PromoteKind;
    use crate::types::PromoteRecord;
    use crate::types::TurnRecord;

    fn turn(objective: &str) -> TurnRecord {
        TurnRecord {
            objective: Some(objective.to_string()),
            ..TurnRecord::default()
        }
    }

    #[tokio::test]
    async fn enqueue_then_flush_delivers_and_empties() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        let op = RetainOp::new(turn("first"));
        outbox.enqueue_retain(&op).unwrap();
        assert_eq!(outbox.depth(), 1);

        let provider = FakeProvider::new();
        let outcome = outbox.flush(&provider).await;
        assert_eq!(outcome.sent, 1);
        assert_eq!(outcome.remaining, 0);
        assert_eq!(outbox.depth(), 0);
        let retained = provider.retained();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].op_id, op.op_id);
    }

    #[tokio::test]
    async fn promote_ops_dispatch_to_the_shared_path() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        let op = PromoteOp::new(
            PromoteKind::Decision,
            PromoteRecord {
                title: "Use sqlite".into(),
                summary: "local first".into(),
                ..PromoteRecord::default()
            },
            None,
        );
        outbox.enqueue_promote(&op).unwrap();
        let provider = FakeProvider::new();
        let outcome = outbox.flush(&provider).await;
        assert_eq!(outcome.sent, 1);
        assert!(provider.retained().is_empty());
        assert_eq!(provider.promoted().len(), 1);
        assert_eq!(provider.promoted()[0].kind, PromoteKind::Decision);
    }

    #[tokio::test]
    async fn a_retry_resends_the_same_op_id_so_the_server_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        let op = RetainOp::new(turn("retry me"));
        outbox.enqueue_retain(&op).unwrap();

        let provider = FakeProvider::new();
        provider.fail_retain(MemoryError::Unavailable("down".into()));
        let first = outbox.flush(&provider).await;
        assert_eq!(first.sent, 0);
        assert!(first.circuit_broken);
        assert_eq!(outbox.depth(), 1);

        // Make it due again, then let it through.
        force_due(&outbox);
        provider.clear_retain_failure();
        let second = outbox.flush(&provider).await;
        assert_eq!(second.sent, 1);

        let retained = provider.retained();
        assert_eq!(retained.len(), 1, "a retry must not create a second record");
        assert_eq!(retained[0].op_id, op.op_id);
    }

    /// Rewrite every entry so it is due now (backoff would otherwise defer it).
    fn force_due(outbox: &Outbox) {
        for path in outbox.list_files() {
            let Some(mut entry) = outbox.read_entry(&path) else {
                continue;
            };
            entry.next_attempt_at_ms = 0;
            outbox.write_entry(&path, &entry);
        }
    }

    #[tokio::test]
    async fn unavailable_schedules_backoff_and_defers_the_rest_of_the_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        for i in 0..3 {
            outbox
                .enqueue_retain(&RetainOp::new(turn(&format!("op {i}"))))
                .unwrap();
        }
        let provider = FakeProvider::new();
        provider.fail_retain(MemoryError::Unavailable("down".into()));
        let outcome = outbox.flush(&provider).await;
        assert_eq!(outcome.sent, 0);
        assert!(outcome.circuit_broken);
        assert_eq!(
            outcome.remaining, 3,
            "nothing is dropped when the server is down"
        );

        // The head entry carries an attempt count and a future due time; the
        // rest were never touched.
        let files = outbox.list_files();
        let head = outbox.read_entry(&files[0]).unwrap();
        assert_eq!(head.attempts, 1);
        assert!(head.next_attempt_at_ms > now_ms());
        let tail = outbox.read_entry(&files[2]).unwrap();
        assert_eq!(tail.attempts, 0);

        // On the next pass the head is still backing off and is skipped, but
        // the entries behind it were never attempted and are due now — one
        // slow op must not stall the whole queue forever.
        provider.clear_retain_failure();
        let next = outbox.flush(&provider).await;
        assert_eq!(next.sent, 2);
        assert_eq!(next.deferred, 1);
        assert_eq!(next.remaining, 1);
    }

    #[test]
    fn backoff_doubles_then_saturates() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(8), Duration::from_secs(256));
        assert_eq!(backoff_delay(9), MAX_BACKOFF);
        assert_eq!(backoff_delay(64), MAX_BACKOFF);
    }

    #[tokio::test]
    async fn auth_failure_keeps_the_op_and_reports_once() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        outbox
            .enqueue_retain(&RetainOp::new(turn("keep me")))
            .unwrap();
        let provider = FakeProvider::new();
        provider.auth_failure();
        let outcome = outbox.flush(&provider).await;
        assert!(outcome.auth_failed);
        assert_eq!(outcome.sent, 0);
        assert_eq!(outcome.dropped, 0);
        assert_eq!(outbox.depth(), 1, "an auth failure must not lose the op");
    }

    #[tokio::test]
    async fn invalid_ops_are_dropped_and_the_pass_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        outbox.enqueue_retain(&RetainOp::new(turn("bad"))).unwrap();
        outbox
            .enqueue_retain(&RetainOp::new(turn("also bad")))
            .unwrap();
        let provider = FakeProvider::new();
        provider.fail_retain(MemoryError::Invalid("400".into()));
        let outcome = outbox.flush(&provider).await;
        assert_eq!(outcome.dropped, 2);
        assert_eq!(outcome.sent, 0);
        assert!(!outcome.circuit_broken);
        assert_eq!(outbox.depth(), 0);
    }

    #[tokio::test]
    async fn corrupt_entries_are_discarded() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("0000000000000001-cm-bad.json"),
            "{ not json",
        )
        .unwrap();
        let provider = FakeProvider::new();
        let outcome = outbox.flush(&provider).await;
        assert_eq!(outcome.dropped, 1);
        assert_eq!(outbox.depth(), 0);
    }

    #[test]
    fn the_bound_drops_the_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path()).with_max_ops(3);
        let mut ids = Vec::new();
        for i in 0..6 {
            let op = RetainOp::new(turn(&format!("op {i}")));
            ids.push(op.op_id.clone());
            outbox.enqueue_retain(&op).unwrap();
        }
        assert_eq!(outbox.depth(), 3);
        // The three survivors are the three most recent op ids.
        let surviving: Vec<String> = outbox
            .list_files()
            .iter()
            .filter_map(|p| outbox.read_entry(p))
            .map(|e| e.op.op_id().to_string())
            .collect();
        for id in &ids[3..] {
            assert!(surviving.contains(id), "recent op {id} was dropped");
        }
        for id in &ids[..3] {
            assert!(
                !surviving.contains(id),
                "old op {id} should have been dropped"
            );
        }
    }

    #[test]
    fn secrets_are_redacted_before_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        let op = RetainOp::new(turn("rotate AKIAIOSFODNN7EXAMPLE now"));
        let path = outbox.enqueue_retain(&op).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("AKIAIOSFODNN7EXAMPLE"), "{on_disk}");
        assert!(on_disk.contains("[REDACTED:aws_access_key]"));
        // The idempotency key survives redaction.
        assert!(on_disk.contains(&op.op_id));
    }

    #[test]
    fn depth_and_oldest_age_report_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path());
        assert_eq!(outbox.depth(), 0);
        assert_eq!(outbox.oldest_age(), None);
        outbox.enqueue_retain(&RetainOp::new(turn("x"))).unwrap();
        assert_eq!(outbox.depth(), 1);
        assert!(outbox.oldest_age().is_some());
    }

    #[test]
    fn per_project_directories_are_isolated() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Outbox::for_project(tmp.path(), "proj/a");
        let b = Outbox::for_project(tmp.path(), "proj-b");
        a.enqueue_retain(&RetainOp::new(turn("a"))).unwrap();
        assert_eq!(a.depth(), 1);
        assert_eq!(b.depth(), 0);
        assert_ne!(a.dir(), b.dir());
    }

    #[test]
    fn project_ids_are_sanitized_into_one_path_component() {
        assert_eq!(sanitize_component("proj/../etc"), "proj_.._etc");
        assert_eq!(sanitize_component(""), "unknown");
        assert_eq!(sanitize_component("ok-123.x_y"), "ok-123.x_y");
    }

    #[test]
    fn entries_round_trip_through_json() {
        let entry = OutboxEntry {
            op: OutboxOp::Retain(RetainOp::new(turn("round trip"))),
            attempts: 2,
            next_attempt_at_ms: 12345,
        };
        let text = serde_json::to_string(&entry).unwrap();
        let back: OutboxEntry = serde_json::from_str(&text).unwrap();
        assert_eq!(back, entry);
        assert_eq!(back.op.kind(), "retain");
    }

    #[tokio::test]
    async fn flushing_an_empty_or_missing_directory_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path().join("never-created"));
        let provider = FakeProvider::new();
        let outcome = outbox.flush(&provider).await;
        assert_eq!(outcome, FlushOutcome::default());
    }
}
