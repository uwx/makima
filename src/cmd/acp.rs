use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_agent::{command, tools::ToolRegistry};
use maki_config::project::{self, TrustMode};
use maki_config::{load_env_files, load_permissions};
use maki_lua::PluginHost;
use maki_lua::ui_sink::HostUi;
use maki_storage::StateDir;

use crate::setup;

/// The CLI theme store: the desktop switches themes through it, so the TUI and
/// the next run agree.
struct UiThemes;

impl maki_acp::ThemeSource for UiThemes {
    fn names(&self) -> Vec<String> {
        maki_ui::theme_names()
    }

    fn current(&self) -> maki_acp::AcpTheme {
        palette()
    }

    fn install(&self, name: &str) -> Result<maki_acp::AcpTheme, String> {
        maki_ui::select_theme(name)?;
        Ok(palette())
    }
}

fn palette() -> maki_acp::AcpTheme {
    let (name, colors) = maki_ui::theme_palette();
    colors
        .into_iter()
        .fold(maki_acp::AcpTheme::new(name), |theme, (role, rgb)| {
            theme.with_color(role, rgb)
        })
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    cwd: PathBuf,
    model_arg: Option<String>,
    yolo: bool,
    automode: bool,
    no_plugins: bool,
    no_jit: bool,
    system_prompt_override: Option<String>,
    append_system_prompt: Option<String>,
    trust_mode: TrustMode,
    listen: Option<SocketAddr>,
    share: Option<SocketAddr>,
    token: Option<String>,
) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    let lan = match (listen, share) {
        (Some(address), _) => Some((address, false)),
        (None, Some(address)) => Some((address, true)),
        (None, None) => None,
    }
    .map(|(address, shared)| -> Result<(SocketAddr, bool, String)> {
        let token = match token {
            Some(token) => token,
            None => lan_token(&storage)?,
        };
        Ok((address, shared, token))
    })
    .transpose()?;
    maki_providers::model_registry::load_from_storage(&storage);
    let _instance = maki_storage::instances::InstanceGuard::register(
        &storage,
        maki_storage::instances::InstanceKind::Acp,
        &cwd,
        None,
    );
    if let Err(error) = &_instance {
        eprintln!("warning: cannot record this instance: {error}");
    }

    let trust = project::resolve(&storage, &cwd, trust_mode);
    load_env_files(&trust.project_config);

    let command_registry = maki_commands::CommandRegistry::new();
    let ui = maki_acp::ui::UiChannel::new();
    let mut plugin_host = PluginHost::with_command_registry(
        Arc::clone(ToolRegistry::global_arc()),
        command_registry.clone(),
        !no_jit,
        HostUi::Sink(Box::new(ui.sink.clone())),
    )
    .context("initialize lua plugin host")?;

    let mut warnings: Vec<String> = trust.warning.clone().into_iter().collect();
    let raw_config = plugin_host
        .load_init_files_or_skip(no_plugins, &trust.project_config, &mut warnings)
        .context("load init.lua files")?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let mut config = raw_config
        .unwrap_or_default()
        .into_config()
        .context("invalid config")?;
    maki_lua::set_allowed_private_hosts(&config.net.allowed_private_hosts);
    config.permissions = load_permissions(&trust.project_config)?;

    if yolo || config.always_yolo {
        config.permissions.yolo = true;
    }
    let automode_on = automode || config.always_automode;
    super::seed_automode(&mut config, automode_on);
    config.validate()?;

    let timeouts = maki_providers::Timeouts::from(&config.provider);

    plugin_host
        .load_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let providers = plugin_host
        .provider_registry(timeouts)
        .context("build provider registry")?;
    let catalog = maki_lua::orchestration::ModelCatalog::offline(
        Arc::clone(&providers),
        &config.provider.model_policy,
    );
    let standard_commands = command::StandardCommands::register(
        &command_registry,
        &command::discover_commands(&cwd, &trust.project_config),
        command::StandardCompletions {
            model: Some(Arc::new(maki_acp::completion::ModelArgSource::new(
                catalog.specs(),
                Arc::clone(&providers),
            ))),
            theme: None,
        },
    )
    .context("register standard commands")?;

    let model = setup::resolve_model(&providers, model_arg.as_deref(), &config.provider, &storage)?;

    setup::init_logging(&config.storage);
    setup::install_panic_log_hook();
    for warning in super::unmatched_permission_rules(&config.permissions) {
        eprintln!("warning: {warning}");
    }

    let lua_event_handle = plugin_host.event_handle();
    let yolo = config.permissions.yolo;

    let params = maki_acp::AcpParams {
        model,
        config: config.agent,
        permissions_config: config.permissions,
        timeouts,
        initial_wd: cwd,
        storage,
        prompt_slots: lua_event_handle.prompt_slot_source(),
        yolo,
        defaults: config.session_defaults,
        system_prompt_override,
        append_system_prompt,
        model_policy: Arc::new(config.provider.model_policy.clone()),
        providers,
        plugin_rules: plugin_host.plugin_rules(),
        lua_event_handle,
        command_registry,
        catalog,
        trust_mode,
        trust_policy: Arc::new(config.trust),
        ui,
        themes: Some(Arc::new(UiThemes)),
    };
    drop(standard_commands);
    match lan {
        Some((address, shared, token)) => {
            eprintln!("ACP listening on {address}; LAN token: {token}");
            if shared {
                maki_acp::run_shared(params, address, token)
            } else {
                maki_acp::run_tcp(params, address, token)
            }
        }
        None => maki_acp::run(params),
    }
}

/// The token a TCP client must present: `--token`, then `MAKIMA_LAN_TOKEN`,
/// then the one stored for this machine, generated on first use.
fn lan_token(storage: &StateDir) -> Result<String> {
    maki_lan::token(storage.path()).context("read the LAN token")
}
