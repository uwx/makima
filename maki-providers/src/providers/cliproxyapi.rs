use std::sync::{Arc, Mutex};

use flume::Sender;
use maki_storage::id::SessionRef;
use serde_json::Value;

use crate::model::{Model, ModelEntry, ModelInfo};
use crate::provider::{BoxFuture, Provider};
use crate::types::{Effort, EffortDialect, ThinkingConfigExt};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{MODELS_PATH, OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyHeader, KeyPool, KeyRotation, ResolvedAuth};

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8317/v1";
/// Any value works here. Asking for a client version switches `/v1/models` to
/// the rich catalog, the only one carrying context windows, modalities and
/// reasoning levels.
const CLIENT_VERSION: &str = "maki";
const REASONING_NONE: &str = "none";

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "cliproxyapi",
    api_key_env: "CLIPROXYAPI_API_KEY",
    base_url: DEFAULT_BASE_URL,
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "CLIProxyAPI",
};

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "cliproxyapi",
    display_name: "CLIProxyAPI",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: DEFAULT_BASE_URL,
    default_api_key_env: "CLIPROXYAPI_API_KEY",
    default_model: "",
    plans: None,
    login_url: None,
    needs_url: true,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}

/// The effort levels the gateway advertises for one model, kept beside the
/// discovered catalog entry so a request only ever asks for a level the model
/// lists (`ultra` and `none` have no makima equivalent and are dropped).
#[derive(Debug)]
pub(crate) struct CliproxyApiModelInfo {
    efforts: Vec<Effort>,
}

pub struct CliproxyApi {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl CliproxyApi {
    /// Construction fails until the gateway is pointed at, so model discovery
    /// does not probe the default port for every user who never mentioned CPA.
    /// Either a key or a configured URL is enough: a deployment can turn auth
    /// off, the same way it can move off the default port.
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let key_pool = KeyPool::resolve(CONFIG.slug, CONFIG.api_key_env).ok();
        let configured = maki_config::providers::configured_base_url(
            CONFIG.slug,
            maki_config::providers::ProvidersConfig::load().get(CONFIG.slug),
        );
        if key_pool.is_none() && configured.is_none() {
            return Err(AgentError::Config {
                message: format!(
                    "{0} not set and no URL configured for '{1}' — start CLIProxyAPI and run `makima auth login {1}`",
                    CONFIG.api_key_env, CONFIG.slug
                ),
            });
        }
        let auth = match key_pool.as_ref().map(KeyPool::current) {
            Some(key) => ResolvedAuth::bearer(CONFIG.slug, key)?,
            None => ResolvedAuth::new(CONFIG.slug, Vec::new())?,
        };
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth: Arc::new(Mutex::new(
                auth.with_base_url(configured.as_deref().map(api_base)),
            )),
            key_pool,
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(auth: Arc<Mutex<ResolvedAuth>>, timeouts: super::Timeouts) -> Self {
        Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts),
            auth,
            key_pool: None,
            system_prefix: None,
        }
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn do_list_models(&self, auth: &ResolvedAuth) -> Result<Vec<ModelInfo>, AgentError> {
        let url = format!(
            "{}{MODELS_PATH}?client_version={CLIENT_VERSION}",
            self.compat.base_url(auth)
        );
        let body: Value = serde_json::from_str(&self.compat.get_text(auth, &url).await?)?;
        Ok(parse_models(&body))
    }
}

/// The port is the whole address CPA users know, but its API lives under
/// `/v1`; a stored bare origin would otherwise 404 on every request.
fn api_base(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn limit(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok().filter(|n| *n > 0)
}

/// Entries arrive either as `{"effort": "high"}` or as the bare string.
fn effort_name(level: &Value) -> &str {
    level
        .get("effort")
        .and_then(Value::as_str)
        .or_else(|| level.as_str())
        .unwrap_or("")
}

fn parse_model(m: &Value) -> Option<ModelInfo> {
    let id = m["slug"].as_str().or_else(|| m["id"].as_str())?.trim();
    if id.is_empty() {
        return None;
    }
    let efforts: Vec<Effort> = m["supported_reasoning_levels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|level| effort_name(level).parse().ok())
        .collect();
    // An empty ladder is "CPA does not say", not "no thinking": it reports
    // nothing for the native Anthropic routes, which do reason when asked.
    let ladder_given = m["supported_reasoning_levels"]
        .as_array()
        .is_some_and(|levels| !levels.is_empty());
    Some(ModelInfo {
        id: id.to_string(),
        context_window: limit(&m["context_window"]),
        max_output_tokens: None,
        pricing: None,
        supports_thinking: ladder_given.then(|| {
            m["supported_reasoning_levels"]
                .as_array()
                .is_some_and(|levels| levels.iter().any(|l| effort_name(l) != REASONING_NONE))
        }),
        supports_vision: m["input_modalities"]
            .as_array()
            .map(|mods| mods.iter().any(|v| v.as_str() == Some("image"))),
        tier: None,
        provider_info: (!efforts.is_empty())
            .then(|| Arc::new(CliproxyApiModelInfo { efforts }) as Arc<_>),
    })
}

/// Accepts both catalog shapes: the rich `models` array with a client version,
/// and the plain OpenAI `data` listing without one.
fn parse_models(body: &Value) -> Vec<ModelInfo> {
    let Some(entries) = body["models"]
        .as_array()
        .or_else(|| body["data"].as_array())
    else {
        return Vec::new();
    };
    let mut models: Vec<ModelInfo> = entries.iter().filter_map(parse_model).collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// The dialected effort ladder for `model`, or the shared fallback when
/// discovery never saw this id. `Adaptive` means "on at a sensible depth", so
/// it lands on `high` snapped into what the model actually accepts.
fn effort_dialect(efforts: &[Effort]) -> EffortDialect<'_> {
    if efforts.is_empty() {
        return dialect::PREFER_HIGH;
    }
    EffortDialect {
        supported: efforts,
        adaptive: Some(Effort::High.snap(efforts)),
        off: None,
    }
}

impl Provider for CliproxyApi {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            let mut buf = String::new();
            let system = super::with_prefix(&self.system_prefix, system, &mut buf);
            let mut body = self.compat.build_body(model, messages, system, tools);
            if model.supports_thinking() {
                let info = crate::model_registry::provider_info::<CliproxyApiModelInfo>(
                    CONFIG.slug,
                    &model.id,
                );
                let efforts = info.as_ref().map_or(&[][..], |i| i.efforts.as_slice());
                opts.thinking
                    .apply_reasoning_effort(&mut body, &effort_dialect(efforts), model);
            }
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(async move {
            let auth = self.auth.lock().unwrap().clone();
            self.do_list_models(&auth).await
        })
    }

    fn keys(&self) -> Option<KeyRotation<'_>> {
        Some(KeyRotation::new(
            self.key_pool.as_ref()?,
            &self.auth,
            KeyHeader::Bearer,
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const MODEL_ID: &str = "glm-5.3-flash";

    const RICH_BODY: &str = r#"{"models": [{
        "slug": "glm-5.3-flash",
        "context_window": 500000,
        "input_modalities": ["text", "image"],
        "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}, {"effort": "max"}]
    }]}"#;

    #[test]
    fn reads_the_rich_catalog_shape() {
        let body: Value = serde_json::from_str(RICH_BODY).unwrap();
        let model = &parse_models(&body)[0];

        assert_eq!(model.id, MODEL_ID);
        assert_eq!(model.context_window, Some(500_000));
        assert_eq!(model.supports_vision, Some(true));
        assert_eq!(model.supports_thinking, Some(true));
    }

    #[test]
    fn reads_the_plain_catalog_shape() {
        let body =
            json!({"data": [{"id": MODEL_ID, "object": "model", "owned_by": "commandcode"}]});

        let model = &parse_models(&body)[0];
        assert_eq!(model.id, MODEL_ID);
        assert_eq!(model.context_window, None);
        assert_eq!(model.supports_thinking, None);
    }

    #[test_case(json!([]), None ; "empty_ladder_is_unknown")]
    #[test_case(json!([{"effort": "none"}]), Some(false) ; "none_only_is_no_thinking")]
    fn thinking_follows_the_reasoning_ladder(levels: Value, expected: Option<bool>) {
        let body = json!({"models": [{"slug": MODEL_ID, "supported_reasoning_levels": levels}]});
        assert_eq!(parse_models(&body)[0].supports_thinking, expected);
    }

    #[test]
    fn zero_limits_are_unknown() {
        let body = json!({"models": [{"slug": MODEL_ID, "context_window": 0}]});
        assert_eq!(parse_models(&body)[0].context_window, None);
    }

    #[test]
    fn api_base_appends_v1_once() {
        assert_eq!(api_base("http://127.0.0.1:8317"), DEFAULT_BASE_URL);
        assert_eq!(api_base("http://127.0.0.1:8317/"), DEFAULT_BASE_URL);
        assert_eq!(api_base(DEFAULT_BASE_URL), DEFAULT_BASE_URL);
    }

    #[test]
    fn the_advertised_ladder_caps_the_effort_sent() {
        let body: Value = serde_json::from_str(RICH_BODY).unwrap();
        let models = parse_models(&body);
        let info = models[0]
            .provider_info
            .as_ref()
            .unwrap()
            .downcast_ref::<CliproxyApiModelInfo>()
            .unwrap();

        let dt = effort_dialect(&info.efforts);
        assert_eq!(
            dt.supported,
            &[Effort::Low, Effort::Medium, Effort::High, Effort::Max]
        );
        assert_eq!(
            dt.adaptive,
            Some(Effort::High),
            "ultra and none are dropped from the ladder"
        );
    }

    #[test]
    fn an_unknown_model_falls_back_to_the_shared_ladder() {
        let dt = effort_dialect(&[]);
        assert_eq!(dt.supported, dialect::PREFER_HIGH.supported);
    }
}
