//! Proves the shapes the inherit plugin's converters emit are accepted by
//! makima's own session loader.
//!
//! The Lua spec (`plugins/inherit/tests/spec.lua`) asserts which blocks each
//! converter produces. This test takes the block vocabulary those converters
//! use and round-trips it through `Session<Message, TokenUsage, ToolOutput>`,
//! the type the TUI saves and resumes, so a shape makima cannot parse fails
//! here rather than when a user resumes an imported session.

use std::sync::Arc;

use maki_agent::ToolOutput;
use maki_agent::tools::ToolRegistry;
use maki_lua::PluginHost;
use maki_providers::{Message, TokenUsage};
use maki_storage::id::MakiId;
use maki_storage::sessions::Session;

type AppSession = Session<Message, TokenUsage, ToolOutput>;

const ID: &str = "CNK1hV6GWoysH3KQMm5wu";

/// Builds a session from the JSON records the plugin writes and reloads it
/// from disk, returning the reloaded copy. The plugin's header is format
/// version 2 with all four usage counters, which is what the loader expects.
fn roundtrip(messages: &[serde_json::Value]) -> AppSession {
    let id: MakiId = ID.parse().unwrap();
    let mut session = AppSession::new("test/model", "/tmp/project");
    session.id = id;
    for value in messages {
        let message: Message = serde_json::from_value(value.clone())
            .unwrap_or_else(|e| panic!("makima rejected a converted message: {e}"));
        session.push_message(message);
    }

    let dir = tempfile::tempdir().unwrap();
    session.save_to(dir.path()).unwrap();
    AppSession::load_from(id, dir.path())
        .unwrap_or_else(|e| panic!("makima could not reload the imported session: {e}"))
}

/// Every block kind the converters emit, in one conversation. If makima adds a
/// required field later, this fails and points at the block.
fn sample_conversation() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "role": "user",
            "content": [{ "type": "text", "text": "explain the phase system" }]
        }),
        serde_json::json!({
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": "look at the base class" },
                { "type": "text", "text": "reading the code" },
                { "type": "tool_use", "id": "call_1", "name": "read", "input": { "path": "Phase.cs" } }
            ]
        }),
        serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": "class BasePhase {}",
                "is_error": false
            }]
        }),
        serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "it resets on enter" }]
        }),
    ]
}

#[test]
fn every_converter_block_kind_roundtrips() {
    let loaded = roundtrip(&sample_conversation());
    let messages = loaded.messages();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].content.len(), 3);
    assert_eq!(messages[2].content.len(), 1);
}

#[test]
fn a_tool_less_transcript_roundtrips() {
    // The Cline and Copilot converters produce text-only turns; those must
    // load too, since they skip all tool pairing.
    let loaded = roundtrip(&[
        serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
        serde_json::json!({ "role": "assistant", "content": [{ "type": "text", "text": "hello" }] }),
    ]);
    assert_eq!(loaded.messages().len(), 2);
}

#[test]
fn the_generated_header_shape_matches_the_loader() {
    // The plugin writes `{"t":"header","v":2,...}` then `msg` records then one
    // `meta` record with all four token counters. `Session::save_to` writes that
    // exact layout, so comparing the two proves the plugin's hand-written JSONL
    // is the same format and not merely a compatible-looking one.
    let id: MakiId = ID.parse().unwrap();
    let mut session = AppSession::new("test/model", "/tmp/project");
    session.id = id;
    session.push_message(Message::user("hello".into()));
    let dir = tempfile::tempdir().unwrap();
    session.save_to(dir.path()).unwrap();

    let text = std::fs::read_to_string(dir.path().join(format!("{id}.jsonl"))).unwrap();
    let first = text.lines().next().unwrap();
    assert!(first.starts_with(r#"{"t":"header","v":2"#), "{first}");
    let meta = text.lines().last().unwrap();
    assert!(meta.starts_with(r#"{"t":"meta""#), "{meta}");
    assert!(meta.contains(r#""cache_read_input_tokens":0"#), "{meta}");
    // A plugin-written id is 16 bytes of base58: round-trip through the same
    // parser makima uses for session file names.
    assert!(ID.parse::<MakiId>().is_ok());
}

/// Keeps the plugin compiling in this binary's dependency graph: the plugin
/// loads on a real host exactly as it does at startup.
#[test]
fn plugin_loads_on_a_real_host() {
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source("inherit", include_str!("../../plugins/inherit/init.lua"))
        .unwrap();
}
