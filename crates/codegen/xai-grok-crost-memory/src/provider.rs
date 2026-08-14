//! The provider abstraction — the ONLY layer that knows Hindsight exists.
//!
//! Everything above it (recall orchestration, retention, promotion, outbox,
//! diagnostics) is written against this trait, which is why the fake provider
//! is a complete substitute in tests.

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::CrostMemoryConfig;
use crate::config::ProviderKind;
use crate::identity::ProjectIdentity;
use crate::types::PromoteOp;
use crate::types::ProviderStatus;
use crate::types::RecallItem;
use crate::types::RecallScope;
use crate::types::RetainOp;

/// Failure classes, chosen for what the caller must DO about them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    /// Network error, timeout, or 5xx. Retry with backoff.
    #[error("memory backend unavailable: {0}")]
    Unavailable(String),
    /// 401/403. Retrying cannot help; surface exactly one visible warning.
    #[error("memory backend rejected our credentials: {0}")]
    Auth(String),
    /// Other 4xx, or a request we should never have built. Drop it and log.
    #[error("memory request rejected: {0}")]
    Invalid(String),
}

impl MemoryError {
    /// True only for [`MemoryError::Unavailable`] — the sole class where the
    /// same bytes sent again might succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    /// Short stable label for tracing fields.
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "unavailable",
            Self::Auth(_) => "auth",
            Self::Invalid(_) => "invalid",
        }
    }
}

/// The provider contract.
///
/// Every implementation must be cancel-safe: dropping any of these futures
/// (a cancelled turn drops in-flight recalls) must leave no partial state.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Read memories from one scope. Never blocks longer than the caller's
    /// timeout — the caller wraps this in `tokio::time::timeout`.
    async fn recall(
        &self,
        scope: RecallScope,
        query: &str,
        max_tokens: usize,
        max_items: usize,
    ) -> Result<Vec<RecallItem>, MemoryError>;

    /// Write a turn record to the agent's PRIVATE bank. Automatic retention
    /// never touches the shared bank.
    async fn retain_private(&self, op: &RetainOp) -> Result<(), MemoryError>;

    /// Write an explicitly promoted record to the SHARED bank. Returns the
    /// record id (the op id, which is also the server-side document id).
    async fn promote_shared(&self, op: &PromoteOp) -> Result<String, MemoryError>;

    /// Liveness probe for diagnostics. Never fails — an unreachable backend is
    /// reported as `healthy: false`.
    async fn status(&self) -> ProviderStatus;
}

/// Build the configured provider for a resolved project.
pub fn build_provider(
    cfg: &CrostMemoryConfig,
    identity: &ProjectIdentity,
) -> Result<Arc<dyn MemoryProvider>, MemoryError> {
    match cfg.provider {
        ProviderKind::Hindsight => {
            let provider = crate::hindsight::HindsightProvider::new(cfg, identity)?;
            Ok(Arc::new(provider))
        }
        ProviderKind::Fake => Ok(Arc::new(crate::fake::FakeProvider::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ProjectIdentity {
        ProjectIdentity {
            project_id: "p1".into(),
            slug: "ohm-storefront".into(),
            bank_prefix: None,
        }
    }

    #[test]
    fn only_unavailable_is_retryable() {
        assert!(MemoryError::Unavailable("boom".into()).is_retryable());
        assert!(!MemoryError::Auth("401".into()).is_retryable());
        assert!(!MemoryError::Invalid("400".into()).is_retryable());
    }

    #[test]
    fn error_classes_are_stable_labels() {
        assert_eq!(
            MemoryError::Unavailable(String::new()).class(),
            "unavailable"
        );
        assert_eq!(MemoryError::Auth(String::new()).class(), "auth");
        assert_eq!(MemoryError::Invalid(String::new()).class(), "invalid");
    }

    #[test]
    fn fake_provider_builds_without_a_base_url() {
        let cfg = CrostMemoryConfig {
            provider: ProviderKind::Fake,
            ..CrostMemoryConfig::default()
        };
        let p = build_provider(&cfg, &identity()).unwrap();
        drop(p);
    }

    #[test]
    fn hindsight_requires_a_base_url() {
        let cfg = CrostMemoryConfig::default();
        let Err(err) = build_provider(&cfg, &identity()) else {
            panic!("hindsight must refuse to build without a base_url");
        };
        assert!(matches!(err, MemoryError::Invalid(_)));
        assert!(err.to_string().contains("base_url"));
    }

    #[test]
    fn hindsight_builds_with_a_base_url() {
        let cfg = CrostMemoryConfig {
            base_url: Some("http://localhost:8080".into()),
            ..CrostMemoryConfig::default()
        };
        assert!(build_provider(&cfg, &identity()).is_ok());
    }
}
