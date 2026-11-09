//! Kind-default completion providers for ACP clients. A client renders the
//! candidates but cannot walk the agent's filesystem, so paths are listed
//! here.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use maki_commands::{
    CancellationToken, CommandCompletion, CommandFuture, CompletionContext, CompletionError,
    CompletionItem, MAX_COMPLETION_CANDIDATES,
};

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
            resolve(cwd, &query[..=index]),
            query[..=index].to_owned(),
            query[index + 1..].to_owned(),
        ),
        None => (cwd.to_path_buf(), String::new(), query.to_owned()),
    }
}

fn resolve(cwd: &Path, value: &str) -> PathBuf {
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
