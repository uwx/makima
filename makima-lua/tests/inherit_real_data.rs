//! Imports a real transcript from this machine through the plugin's converter
//! and reloads it with makima's own session loader.
//!
//! The other inherit tests build their own fixtures. This one reads whatever
//! another harness actually wrote (Claude Code first, then any other installed
//! source) so a change in the on-disk format shows up as a failed load rather
//! than a silent import of nothing. It skips, without failing, on a machine
//! that has no foreign sessions.

use std::sync::Arc;

use makima_agent::ToolOutput;
use makima_agent::tools::ToolRegistry;
use makima_lua::PluginHost;
use makima_providers::{Message, TokenUsage};
use makima_storage::id::MakimaId;
use makima_storage::sessions::Session;

type AppSession = Session<Message, TokenUsage, ToolOutput>;

const ID: &str = "CNK1hV6GWoysH3KQMm5wu";
const SOURCES: &[&str] = &[
    "claude", "omp", "codex", "gemini", "continue", "cline", "copilot",
];

/// Runs the plugin's discovery and conversion over the first transcript of
/// `source` and returns the JSONL text it would write, or nil when this
/// machine has none.
fn convert_source(source: &str) -> Option<String> {
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    let out = std::env::temp_dir().join(format!("inherit_real_{source}.jsonl"));
    let _ = std::fs::remove_file(&out);
    let cwd = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let script = format!(
        r#"
local H = load_helpers()
local T = load_transcripts()
local found = H.list_conversations([==[{cwd}]==], [==[{source}]==])
-- Newest first: editors leave empty placeholder shells, so try a handful.
for i = 1, math.min(#found, 20) do
  local text = makima.fs.read(found[i].path)
  if text and #text <= H.MAX_TRANSCRIPT_BYTES then
    -- Mirror the importer: the transcript's own project wins over the
    -- directory the import ran in.
    local converted = T.{source}(text)
    local project = H.dir_from_uri(converted.cwd or found[i].cwd) or [==[{cwd}]==]
    local jsonl = T.session_jsonl(converted, [==[{id}]==], project, "test/model", 1700000000)
    if jsonl then
      makima.fs.write([==[{out}]==], jsonl)
      return
    end
  end
end
"#,
        source = source,
        id = ID,
        cwd = cwd,
        out = out.to_string_lossy().replace('\\', "/"),
    );
    let wrapped = format!(
        "load_helpers = function()\n{}\nend\nload_transcripts = function()\n{}\nend\n{}",
        include_str!("../../plugins/inherit/inherit_helpers.lua"),
        include_str!("../../plugins/inherit/inherit_transcripts.lua"),
        script,
    );
    host.load_source("inherit_real", &wrapped).unwrap();
    std::fs::read_to_string(&out).ok()
}

#[test]
fn real_transcript_from_another_harness_reloads() {
    let mut tested = Vec::new();
    for source in SOURCES {
        let Some(jsonl) = convert_source(source) else {
            continue;
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(format!("{ID}.jsonl")), &jsonl).unwrap();
        let id: MakimaId = ID.parse().unwrap();
        let session = AppSession::load_from(id, dir.path())
            .unwrap_or_else(|e| panic!("{source}: imported session did not reload: {e}"));
        // The session scanner counts messages by the literal prefix; a file it
        // cannot count is one `makima sessions` shows with zero messages.
        let summaries = AppSession::list_in(&session.cwd, dir.path()).unwrap();
        let summary = summaries.iter().find(|s| s.id == id).unwrap();
        assert!(
            summary.message_count > 0,
            "{source}: imported session listed zero messages"
        );
        tested.push(*source);
    }
    if tested.is_empty() {
        eprintln!("skipped: no foreign transcripts on this machine");
    } else {
        eprintln!("reloaded real transcripts from: {tested:?}");
    }
}
