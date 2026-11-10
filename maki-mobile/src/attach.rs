//! Where this window attaches.
//!
//! The phone has no terminal to read a link in, so the app owns a small file:
//! `attach.json` in its data dir, holding the link and the token the sharing
//! machine shows. The setup screen writes it, and every later start reads it.

use std::path::Path;

use maki_acp_client::Target;
use serde::{Deserialize, Serialize};

const ATTACH_FILE: &str = "attach.json";

#[derive(Clone, Deserialize, Serialize)]
pub struct Attach {
    pub link: String,
    pub token: String,
}

impl Attach {
    pub fn read(files: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(files.join(ATTACH_FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn target(&self) -> Result<Target, String> {
        Target::lan(&self.link, Some(&self.token))
    }

    /// Keeps the link and the token for the next start.
    pub fn save(&self, files: &Path) -> Result<(), String> {
        self.target()?;
        std::fs::create_dir_all(files)
            .map_err(|error| format!("create {}: {error}", files.display()))?;
        let path = files.join(ATTACH_FILE);
        let text = serde_json::to_string(self)
            .map_err(|error| format!("save {}: {error}", path.display()))?;
        std::fs::write(&path, text).map_err(|error| format!("write {}: {error}", path.display()))
    }
}
