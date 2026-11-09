//! Kind-default completion providers for ACP clients. A client renders the
//! candidates but cannot walk the agent's filesystem, so paths are listed
//! here.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;
use maki_agent::argument_completion::{model_argument_items, plan_model_argument};
use maki_commands::{
    CancellationToken, CommandCompletion, CommandFuture, CompletionContext, CompletionError,
    CompletionItem, MAX_COMPLETION_CANDIDATES,
};
use maki_providers::provider::adjust_model;
use maki_providers::{Model, ModelCapabilities, Timeouts};
use tracing::debug;

/// Completes `/model`: the catalog's specs, then the `key=value` fields the
/// named model accepts. With no model named the host cannot tell which model
/// is current, so every preset is offered and the command validates on run.
#[derive(Clone)]
pub struct ModelArgSource {
    specs: Arc<ArcSwapOption<Vec<String>>>,
    capabilities: Arc<Mutex<HashMap<String, ModelCapabilities>>>,
    timeouts: Timeouts,
}

impl ModelArgSource {
    pub fn new(specs: Arc<ArcSwapOption<Vec<String>>>, timeouts: Timeouts) -> Self {
        Self {
            specs,
            capabilities: Arc::default(),
            timeouts,
        }
    }
}

impl CommandCompletion for ModelArgSource {
    fn complete(
        &self,
        context: CompletionContext,
        _cancellation: CancellationToken,
    ) -> CommandFuture<Result<Vec<CompletionItem>, CompletionError>> {
        let plan = plan_model_argument(
            &context.arguments,
            &context.argument,
            context.argument_range.as_ref(),
            &context.preceding_arguments,
        );
        let specs = self.specs.load_full().map_or_else(Vec::new, |specs| {
            specs
                .iter()
                .map(|spec| Arc::from(spec.as_str()))
                .collect::<Vec<_>>()
        });
        let named = plan.named.clone();
        let cached = named.as_ref().and_then(|spec| {
            self.capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(spec.as_ref())
                .copied()
        });
        let cache = Arc::clone(&self.capabilities);
        let timeouts = self.timeouts;
        Box::pin(async move {
            let resolved = match (named, cached) {
                (Some(_), Some(capabilities)) => Some(capabilities),
                (Some(spec), None) => {
                    let argument = Arc::clone(&spec);
                    let capabilities = smol::unblock(move || resolve(&argument, timeouts)).await;
                    cache
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(spec.to_string(), capabilities);
                    Some(capabilities)
                }
                (None, _) => None,
            };
            Ok(model_argument_items(&specs, &plan, resolved, true))
        })
    }
}

/// Falls back to the static capabilities when the provider cannot be built,
/// so a broken provider shows what the model table knows.
fn resolve(spec: &str, timeouts: Timeouts) -> ModelCapabilities {
    let Ok(mut model) = Model::from_spec(spec) else {
        return ModelCapabilities::NONE;
    };
    if let Err(error) = adjust_model(&mut model, timeouts) {
        debug!(spec, %error, "model capabilities fall back to static metadata");
    }
    ModelCapabilities::from(&model)
}

/// Lists one directory: its entries that start with the typed leaf.
pub struct PathSource {
    files: bool,
}

impl PathSource {
    /// Offers directories only, for [`ArgumentKind::Directory`].
    ///
    /// [`ArgumentKind::Directory`]: maki_commands::ArgumentKind::Directory
    pub fn directories() -> Self {
        Self { files: false }
    }

    /// Offers files and directories, for [`ArgumentKind::File`].
    ///
    /// [`ArgumentKind::File`]: maki_commands::ArgumentKind::File
    pub fn files() -> Self {
        Self { files: true }
    }
}

impl CommandCompletion for PathSource {
    fn complete(
        &self,
        context: CompletionContext,
        cancellation: CancellationToken,
    ) -> CommandFuture<Result<Vec<CompletionItem>, CompletionError>> {
        let cwd = PathBuf::from(context.cwd.as_ref());
        let query = context.argument.to_string();
        let files = self.files;
        Box::pin(async move {
            smol::unblock(move || entries(&cwd, &query, files, &|| cancellation.is_cancelled()))
                .await
                .map_err(|_| CompletionError::Unavailable)
        })
    }
}

/// The directory to list, the text every insertion starts with, and the leaf
/// the entries must start with.
fn split(cwd: &Path, query: &str) -> (PathBuf, String, String) {
    if query.is_empty() {
        return (cwd.to_path_buf(), String::new(), String::new());
    }
    if query == "~" {
        return (
            maki_storage::paths::home().unwrap_or_else(|| cwd.to_path_buf()),
            format!("~{0}", std::path::MAIN_SEPARATOR),
            String::new(),
        );
    }
    match query.rfind(std::path::is_separator) {
        Some(index) => (
            resolve_query(cwd, &query[..=index]),
            query[..=index].to_owned(),
            query[index + 1..].to_owned(),
        ),
        None => (cwd.to_path_buf(), String::new(), query.to_owned()),
    }
}

fn resolve_query(cwd: &Path, value: &str) -> PathBuf {
    let expanded = match value.strip_prefix("~/") {
        Some(rest) => {
            maki_storage::paths::home().map_or_else(|| PathBuf::from(value), |home| home.join(rest))
        }
        None if value == "~" => maki_storage::paths::home().unwrap_or_else(|| PathBuf::from(value)),
        None => PathBuf::from(value),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

fn entries(
    cwd: &Path,
    query: &str,
    files: bool,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Vec<CompletionItem>> {
    let (directory, display, leaf) = split(cwd, query);
    let mut items = Vec::new();
    for entry in std::fs::read_dir(&directory)? {
        if cancelled() {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if leaf.is_empty() && name.starts_with('.') {
            continue;
        }
        if !name.starts_with(&leaf) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() && !files {
            continue;
        }
        let mut insertion = format!("{display}{name}");
        if kind.is_dir() {
            insertion.push(std::path::MAIN_SEPARATOR);
        }
        items.push(CompletionItem {
            label: Arc::from(insertion.as_str()),
            insertion: Arc::from(insertion),
            description: None,
        });
        if items.len() == MAX_COMPLETION_CANDIDATES {
            break;
        }
    }
    items.sort_by(|left, right| left.insertion.cmp(&right.insertion));
    Ok(items)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::entries;

    fn listing(cwd: &std::path::Path, query: &str, files: bool) -> Vec<String> {
        entries(cwd, query, files, &|| false)
            .unwrap()
            .into_iter()
            .map(|item| item.insertion.to_string())
            .collect()
    }

    #[test]
    fn empty_query_lists_directories_only() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        assert_eq!(
            listing(dir.path(), "", false),
            [format!("src{0}", std::path::MAIN_SEPARATOR)]
        );
    }

    #[test]
    fn file_kind_lists_files_and_directories() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        assert_eq!(
            listing(dir.path(), "", true),
            [
                "Cargo.toml".to_owned(),
                format!("src{0}", std::path::MAIN_SEPARATOR)
            ]
        );
    }

    #[test]
    fn typed_leaf_filters_and_keeps_the_display_prefix() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "").unwrap();

        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(listing(dir.path(), "s", false), [format!("src{separator}")]);
        assert_eq!(
            listing(dir.path(), &format!("src{separator}m"), true),
            [format!("src{separator}main.rs")]
        );
    }

    #[test]
    fn missing_directory_is_unavailable() {
        let dir = TempDir::new().unwrap();
        let error = entries(dir.path(), "absent/", false, &|| false).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
