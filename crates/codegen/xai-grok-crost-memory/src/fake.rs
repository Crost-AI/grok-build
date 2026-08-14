//! In-memory provider double.
//!
//! Exposed (not `#[cfg(test)]`) so the shell's own integration tests can drive
//! the whole memory stack — orchestrator, outbox, diagnostics — without a
//! server. Every knob is deterministic; nothing here reads the clock except
//! the explicitly configured `delay`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use async_trait::async_trait;

use crate::provider::MemoryError;
use crate::provider::MemoryProvider;
use crate::types::PromoteOp;
use crate::types::ProviderStatus;
use crate::types::RecallItem;
use crate::types::RecallScope;
use crate::types::RetainOp;

/// Provider label used in status output.
pub const PROVIDER_NAME: &str = "fake";

/// Everything the fake remembers and everything it has been told to do.
#[derive(Debug, Default)]
pub struct FakeState {
    /// Seeded recall results, per scope.
    pub seeded: HashMap<RecallScope, Vec<RecallItem>>,
    /// Every retain op the fake accepted, in order.
    pub retained: Vec<RetainOp>,
    /// Every promote op the fake accepted, in order.
    pub promoted: Vec<PromoteOp>,
    /// Forced recall failure, per scope.
    pub recall_failure: HashMap<RecallScope, MemoryError>,
    /// Forced retain failure.
    pub retain_failure: Option<MemoryError>,
    /// Forced promote failure.
    pub promote_failure: Option<MemoryError>,
    /// Artificial latency, per scope.
    pub delay: HashMap<RecallScope, Duration>,
    /// Reported by `status()`.
    pub healthy: bool,
    /// Count of recall calls, per scope — lets tests assert that a cancelled
    /// or short-circuited path really did not call through.
    pub recall_calls: HashMap<RecallScope, usize>,
}

/// Test double for [`MemoryProvider`].
#[derive(Debug, Clone)]
pub struct FakeProvider {
    state: Arc<Mutex<FakeState>>,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProvider {
    /// A healthy fake with no seeded results and no forced failures.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                healthy: true,
                ..FakeState::default()
            })),
        }
    }

    /// Shared handle to the state, for assertions and for knobs.
    pub fn state(&self) -> Arc<Mutex<FakeState>> {
        Arc::clone(&self.state)
    }

    /// Lock without ever panicking: a poisoned mutex still has usable state,
    /// and a test double is not where a panic should originate.
    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Seed the results a scope will return.
    pub fn seed(&self, scope: RecallScope, items: Vec<RecallItem>) -> &Self {
        self.lock().seeded.insert(scope, items);
        self
    }

    /// Make one scope's recall fail.
    pub fn fail_recall(&self, scope: RecallScope, err: MemoryError) -> &Self {
        self.lock().recall_failure.insert(scope, err);
        self
    }

    /// Clear a forced recall failure.
    pub fn clear_recall_failure(&self, scope: RecallScope) -> &Self {
        self.lock().recall_failure.remove(&scope);
        self
    }

    /// Make retain fail.
    pub fn fail_retain(&self, err: MemoryError) -> &Self {
        self.lock().retain_failure = Some(err);
        self
    }

    /// Clear a forced retain failure.
    pub fn clear_retain_failure(&self) -> &Self {
        self.lock().retain_failure = None;
        self
    }

    /// Make promote fail.
    pub fn fail_promote(&self, err: MemoryError) -> &Self {
        self.lock().promote_failure = Some(err);
        self
    }

    /// Fail everything with [`MemoryError::Auth`], as a rejected key would.
    pub fn auth_failure(&self) -> &Self {
        let err = MemoryError::Auth("fake: rejected credentials".to_string());
        let mut st = self.lock();
        st.recall_failure.insert(RecallScope::Private, err.clone());
        st.recall_failure.insert(RecallScope::Shared, err.clone());
        st.retain_failure = Some(err.clone());
        st.promote_failure = Some(err);
        drop(st);
        self
    }

    /// Delay one scope's recall by `d`.
    pub fn delay(&self, scope: RecallScope, d: Duration) -> &Self {
        self.lock().delay.insert(scope, d);
        self
    }

    /// Set what `status()` reports.
    pub fn set_healthy(&self, healthy: bool) -> &Self {
        self.lock().healthy = healthy;
        self
    }

    /// Snapshot of accepted retain ops.
    pub fn retained(&self) -> Vec<RetainOp> {
        self.lock().retained.clone()
    }

    /// Snapshot of accepted promote ops.
    pub fn promoted(&self) -> Vec<PromoteOp> {
        self.lock().promoted.clone()
    }

    /// How many times a scope's recall was entered.
    pub fn recall_calls(&self, scope: RecallScope) -> usize {
        self.lock().recall_calls.get(&scope).copied().unwrap_or(0)
    }
}

#[async_trait]
impl MemoryProvider for FakeProvider {
    async fn recall(
        &self,
        scope: RecallScope,
        _query: &str,
        _max_tokens: usize,
        max_items: usize,
    ) -> Result<Vec<RecallItem>, MemoryError> {
        // Read every knob under the lock, then release it before awaiting.
        // Holding a std Mutex across an await would make this future !Send and
        // is exactly the pattern a test double should not teach.
        let (delay, failure, items) = {
            let mut st = self.lock();
            *st.recall_calls.entry(scope).or_insert(0) += 1;
            (
                st.delay.get(&scope).copied(),
                st.recall_failure.get(&scope).cloned(),
                st.seeded.get(&scope).cloned().unwrap_or_default(),
            )
        };
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        if let Some(err) = failure {
            return Err(err);
        }
        Ok(items.into_iter().take(max_items).collect())
    }

    async fn retain_private(&self, op: &RetainOp) -> Result<(), MemoryError> {
        let mut st = self.lock();
        if let Some(err) = st.retain_failure.clone() {
            return Err(err);
        }
        st.retained.push(op.clone());
        Ok(())
    }

    async fn promote_shared(&self, op: &PromoteOp) -> Result<String, MemoryError> {
        let mut st = self.lock();
        if let Some(err) = st.promote_failure.clone() {
            return Err(err);
        }
        st.promoted.push(op.clone());
        Ok(op.op_id.clone())
    }

    async fn status(&self) -> ProviderStatus {
        let healthy = self.lock().healthy;
        ProviderStatus {
            healthy,
            provider: PROVIDER_NAME,
            endpoint: None,
            latency_ms: Some(0),
            detail: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TurnRecord;

    fn items(n: usize) -> Vec<RecallItem> {
        (0..n)
            .map(|i| RecallItem::new(format!("m{i}"), format!("content {i}"), 1.0 - (i as f64)))
            .collect()
    }

    #[tokio::test]
    async fn recall_returns_seeded_items_per_scope() {
        let p = FakeProvider::new();
        p.seed(RecallScope::Private, items(2));
        p.seed(RecallScope::Shared, items(1));
        let private = p.recall(RecallScope::Private, "q", 100, 8).await.unwrap();
        let shared = p.recall(RecallScope::Shared, "q", 100, 8).await.unwrap();
        assert_eq!(private.len(), 2);
        assert_eq!(shared.len(), 1);
    }

    #[tokio::test]
    async fn unseeded_scope_is_empty_not_an_error() {
        let p = FakeProvider::new();
        assert!(
            p.recall(RecallScope::Shared, "q", 100, 8)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn max_items_is_honored() {
        let p = FakeProvider::new();
        p.seed(RecallScope::Private, items(10));
        let got = p.recall(RecallScope::Private, "q", 100, 3).await.unwrap();
        assert_eq!(got.len(), 3);
    }

    #[tokio::test]
    async fn forced_failures_are_scope_local() {
        let p = FakeProvider::new();
        p.seed(RecallScope::Shared, items(1));
        p.fail_recall(
            RecallScope::Private,
            MemoryError::Unavailable("down".into()),
        );
        assert!(p.recall(RecallScope::Private, "q", 100, 8).await.is_err());
        assert_eq!(
            p.recall(RecallScope::Shared, "q", 100, 8)
                .await
                .unwrap()
                .len(),
            1
        );
        p.clear_recall_failure(RecallScope::Private);
        assert!(p.recall(RecallScope::Private, "q", 100, 8).await.is_ok());
    }

    #[tokio::test]
    async fn retain_and_promote_are_recorded_in_order() {
        let p = FakeProvider::new();
        let a = RetainOp::new(TurnRecord::default());
        let b = RetainOp::new(TurnRecord::default());
        p.retain_private(&a).await.unwrap();
        p.retain_private(&b).await.unwrap();
        let got = p.retained();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].op_id, a.op_id);
        assert_eq!(got[1].op_id, b.op_id);

        let promo = PromoteOp::new(
            crate::types::PromoteKind::Result,
            crate::types::PromoteRecord::default(),
            None,
        );
        let id = p.promote_shared(&promo).await.unwrap();
        assert_eq!(id, promo.op_id);
        assert_eq!(p.promoted().len(), 1);
    }

    #[tokio::test]
    async fn failed_writes_are_not_recorded() {
        let p = FakeProvider::new();
        p.fail_retain(MemoryError::Unavailable("down".into()));
        assert!(
            p.retain_private(&RetainOp::new(TurnRecord::default()))
                .await
                .is_err()
        );
        assert!(p.retained().is_empty());
        p.clear_retain_failure();
        assert!(
            p.retain_private(&RetainOp::new(TurnRecord::default()))
                .await
                .is_ok()
        );
        assert_eq!(p.retained().len(), 1);
    }

    #[tokio::test]
    async fn auth_failure_mode_covers_every_operation() {
        let p = FakeProvider::new();
        p.auth_failure();
        assert!(matches!(
            p.recall(RecallScope::Private, "q", 100, 8).await,
            Err(MemoryError::Auth(_))
        ));
        assert!(matches!(
            p.recall(RecallScope::Shared, "q", 100, 8).await,
            Err(MemoryError::Auth(_))
        ));
        assert!(matches!(
            p.retain_private(&RetainOp::new(TurnRecord::default()))
                .await,
            Err(MemoryError::Auth(_))
        ));
    }

    #[tokio::test]
    async fn delay_knob_actually_delays() {
        let p = FakeProvider::new();
        p.delay(RecallScope::Private, Duration::from_millis(40));
        let started = std::time::Instant::now();
        let _ = p.recall(RecallScope::Private, "q", 100, 8).await;
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn recall_calls_are_counted() {
        let p = FakeProvider::new();
        assert_eq!(p.recall_calls(RecallScope::Shared), 0);
        let _ = p.recall(RecallScope::Shared, "q", 100, 8).await;
        let _ = p.recall(RecallScope::Shared, "q", 100, 8).await;
        assert_eq!(p.recall_calls(RecallScope::Shared), 2);
    }

    #[tokio::test]
    async fn status_reflects_the_health_knob() {
        let p = FakeProvider::new();
        assert!(p.status().await.healthy);
        p.set_healthy(false);
        let st = p.status().await;
        assert!(!st.healthy);
        assert_eq!(st.provider, "fake");
    }
}
