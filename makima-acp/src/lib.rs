pub mod attach;
pub mod completion;
pub mod elicitation;
pub mod methods;
pub mod permissions;
pub mod server;
pub mod share;
pub mod translate;
pub mod ui;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use makima_agent::agent::PromptSlotSource;
use makima_agent::permissions::PluginRuleStore;
use makima_agent::{AgentConfig, PermissionsConfig};
use makima_commands::CommandRegistry;
use makima_config::{ModelPolicy, SessionDefaults, TrustConfig, project::TrustMode};
use makima_lua::orchestration::ModelCatalog;
use makima_providers::model::Model;
use serde_json::{Map, Value, json};

use crate::ui::UiChannel;

/// The palette a client should paint with. Sent in `initialize`'s
/// `agentCapabilities._meta`, so a client that ignores it renders its own.
#[derive(Clone, Debug, Default)]
pub struct AcpTheme {
    name: String,
    colors: BTreeMap<String, String>,
}

impl AcpTheme {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            colors: BTreeMap::new(),
        }
    }

    /// Adds one role as `#rrggbb`; clients ignore roles they do not know.
    pub fn with_color(mut self, role: &str, [red, green, blue]: [u8; 3]) -> Self {
        self.colors
            .insert(role.to_owned(), format!("#{red:02x}{green:02x}{blue:02x}"));
        self
    }

    pub(crate) fn json(&self) -> Value {
        json!({ "name": self.name, "colors": self.colors })
    }

    pub(crate) fn meta(&self, names: &[String]) -> Map<String, Value> {
        let mut makima = Map::new();
        makima.insert("themes".to_owned(), json!(names));
        makima.insert("theme".to_owned(), self.json());
        let mut meta = Map::new();
        meta.insert("makima".to_owned(), Value::Object(makima));
        meta
    }
}

/// Where a host keeps its themes. The agent answers palette requests with it,
/// so a frontend can switch themes live and the CLI stays in sync.
pub trait ThemeSource: Send + Sync + 'static {
    fn names(&self) -> Vec<String>;
    fn current(&self) -> AcpTheme;
    fn install(&self, name: &str) -> Result<AcpTheme, String>;
}

/// Client to server: install a named theme and answer with its palette.
pub const THEME_METHOD: &str = "makima/theme";

#[derive(serde::Deserialize)]
pub(crate) struct ThemeParams {
    pub(crate) name: String,
}
use makima_providers::{ProviderRegistry, Timeouts};

pub struct AcpParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub storage: makima_storage::StateDir,
    pub prompt_slots: PromptSlotSource,
    pub yolo: bool,
    pub defaults: SessionDefaults,
    pub system_prompt_override: Option<String>,
    pub append_system_prompt: Option<String>,
    pub model_policy: Arc<ModelPolicy>,
    pub providers: Arc<ProviderRegistry>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub lua_event_handle: makima_lua::EventHandle,
    pub command_registry: CommandRegistry,
    /// Shared with the model completion source, so discovery reaches it.
    pub catalog: ModelCatalog,
    pub trust_mode: TrustMode,
    pub trust_policy: Arc<TrustConfig>,
    pub ui: UiChannel,
    pub themes: Option<Arc<dyn ThemeSource>>,
}

pub fn run(params: AcpParams) -> color_eyre::Result<()> {
    smol::block_on(server::serve(params))
}

/// Serves ACP over TCP at {listen}, requiring {token} in each connection's
/// opening handshake.
pub fn run_tcp(
    params: AcpParams,
    listen: std::net::SocketAddr,
    token: String,
) -> color_eyre::Result<()> {
    smol::block_on(server::serve_tcp(params, listen, token))
}

/// Serves ACP over stdio and shares the same session over TCP at {listen}.
pub fn run_shared(
    params: AcpParams,
    listen: std::net::SocketAddr,
    token: String,
) -> color_eyre::Result<()> {
    smol::block_on(server::serve_shared(params, listen, token))
}
