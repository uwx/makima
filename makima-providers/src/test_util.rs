//! Registry fakes for tests that need providers without plugins or network.

use std::sync::Arc;

use serde_json::Value;

use crate::login::{AuthSpec, AuthState, AuthStatus, InteractionService};
use crate::provider::{BoxFuture, unconfigured};
use crate::{
    AgentError, Model, ModelInfo, PreparedProvider, ProviderBackend, ProviderRegistry,
    RegisteredProvider,
};

struct OfflineBackend {
    auth: Option<AuthState>,
}

impl ProviderBackend for OfflineBackend {
    fn prepare(
        &self,
        model: Model,
        _info: ModelInfo,
    ) -> BoxFuture<'static, Result<PreparedProvider, AgentError>> {
        Box::pin(async move {
            Ok(PreparedProvider {
                model,
                provider: unconfigured(),
            })
        })
    }

    fn discover(
        &self,
        _refresh: bool,
    ) -> Option<BoxFuture<'static, Result<Vec<ModelInfo>, AgentError>>> {
        None
    }

    fn login(
        &self,
        _method: String,
        _config: Value,
        _ui: Arc<dyn InteractionService>,
    ) -> BoxFuture<'static, Result<Option<String>, AgentError>> {
        Box::pin(async { Ok(None) })
    }

    fn auth_status(
        &self,
        _config: Value,
    ) -> Option<BoxFuture<'static, Result<AuthStatus, AgentError>>> {
        let auth = self.auth?;
        Some(Box::pin(async move {
            Ok(AuthStatus {
                state: auth,
                detail: None,
            })
        }))
    }

    fn logout(&self) -> Option<BoxFuture<'static, Result<(), AgentError>>> {
        None
    }
}

/// A registry of {providers}, each an id with its curated models and the id
/// as its display name. Any model id of a listed provider resolves; `prepare`
/// hands out a provider that fails each request, and each provider reports
/// {auth} to the auth checks.
pub fn offline_registry_with_auth(
    providers: Vec<(&str, Vec<ModelInfo>)>,
    auth: Option<AuthState>,
) -> ProviderRegistry {
    ProviderRegistry::new(
        providers
            .into_iter()
            .map(|(id, models)| RegisteredProvider {
                id: id.into(),
                display_name: id.into(),
                models,
                backend: Arc::new(OfflineBackend { auth }),
                auth: AuthSpec::default(),
                docs: None,
            })
            .collect(),
    )
}

/// [`offline_registry`] with no provider reporting credentials.
pub fn offline_registry(providers: Vec<(&str, Vec<ModelInfo>)>) -> ProviderRegistry {
    offline_registry_with_auth(providers, None)
}

/// [`offline_ids`] where every provider reports a saved login.
pub fn configured_ids(ids: &[&str]) -> ProviderRegistry {
    offline_registry_with_auth(
        ids.iter().map(|id| (*id, Vec::new())).collect(),
        Some(AuthState::Saved),
    )
}

/// [`offline_registry`] of {ids} without curated models.
pub fn offline_ids(ids: &[&str]) -> ProviderRegistry {
    offline_registry(ids.iter().map(|id| (*id, Vec::new())).collect())
}
