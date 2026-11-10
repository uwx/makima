//! Where this window attaches.
//!
//! The phone has no terminal to read a link from and no keyboard to type one
//! on, so the app owns a small file: `attach.json` in its private data dir,
//! holding the link and the token the sharing machine shows.

use std::path::Path;

use maki_acp_client::Target;
use serde::Deserialize;

const ATTACH_FILE: &str = "attach.json";

#[derive(Deserialize)]
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
}
