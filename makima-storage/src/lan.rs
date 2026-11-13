//! The token LAN clients present to reach this machine's agents: one token per
//! state directory, generated on first use and shared by every listener.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use getrandom::fill;

use crate::atomic_write_permissions;

pub const TOKEN_FILE: &str = "lan-token";

const TOKEN_BYTES: usize = 16;
const TOKEN_MODE: u32 = 0o600;

pub fn path(state: &Path) -> PathBuf {
    state.join(TOKEN_FILE)
}

/// The stored token, generating and saving one when the file is missing or
/// empty, so a listener started later keeps the same pairing.
pub fn token(state: &Path) -> io::Result<String> {
    let path = path(state);
    if let Some(token) = read(&path)? {
        return Ok(token);
    }
    let token = generate()?;
    atomic_write_permissions(&path, token.as_bytes(), TOKEN_MODE)
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(token)
}

fn read(path: &Path) -> io::Result<Option<String>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let token = String::from_utf8_lossy(&data).trim().to_owned();
    Ok((!token.is_empty()).then_some(token))
}

fn generate() -> io::Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    fill(&mut bytes).map_err(|error| io::Error::other(format!("generate LAN token: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_generated_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let first = token(dir.path()).unwrap();
        assert_eq!(first.len(), TOKEN_BYTES * 2);
        assert_eq!(token(dir.path()).unwrap(), first);
    }

    #[test]
    fn an_empty_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(path(dir.path()), "\n").unwrap();
        let generated = token(dir.path()).unwrap();
        assert!(!generated.is_empty());
        assert_eq!(fs::read_to_string(path(dir.path())).unwrap(), generated);
    }
}
