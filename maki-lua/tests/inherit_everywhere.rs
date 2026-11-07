//! Runs the plugin's "every conversation, each into its own project" path
//! against whatever foreign transcripts this machine has.
//!
//! The point is the two things that are easy to get wrong: a successful import
//! must be counted as imported (the writer returns `(id, title)`, and treating
//! any second value as a failure silently reported every success as a skip),
//! and each written session must carry the directory its transcript recorded,
//! not the one the import ran in.
//!
//! It skips, without failing, on a machine with no foreign sessions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::tools::ToolRegistry;
use maki_lua::test_support::run_command;
use maki_lua::{PluginHost, UiAction};

#[test]
fn everywhere_imports_each_chat_into_its_own_project() {
    let state = tempfile::tempdir().unwrap();
    // Session writes go through the state dir; point it at the tempdir so the
    // test never touches the real session store.
    if maki_storage::paths::init_at(state.path().to_path_buf()).is_err() {
        eprintln!("skipped: could not redirect the state directory");
        return;
    }
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source("inherit", include_str!("../../plugins/inherit/init.lua"))
        .unwrap();

    let rx = host.ui_action_rx();
    let completion = run_command(&host.command_registry(), "/inherit conversations", 0, None);
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut summary = None;
    loop {
        if completion.try_recv().is_ok() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UiAction::Flash(msg)) => summary = Some(msg),
            Ok(UiAction::OpenListPicker { .. }) => {
                panic!("/inherit conversations must not open the picker")
            }
            Ok(_) => {}
            Err(flume::RecvTimeoutError::Timeout) if Instant::now() > deadline => break,
            Err(flume::RecvTimeoutError::Disconnected) => break,
            Err(_) => {}
        }
    }
    let summary = summary.expect("/inherit conversations produced no summary");
    eprintln!("{summary}");

    let sessions = state.path().join("sessions");
    let mut written = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let first = std::fs::read_to_string(&path).unwrap_or_default();
            let cwd = first
                .split("\"cwd\":\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default()
                .to_owned();
            written.push(cwd);
        }
    }

    if written.is_empty() {
        eprintln!("skipped: no foreign transcripts on this machine");
        return;
    }
    // The tally must agree with what actually landed on disk: a success is an
    // import, not a skip.
    let reported: usize = summary
        .split("conversations: ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    assert_eq!(
        reported,
        written.len(),
        "tally says {reported} imported but {} sessions were written: {summary}",
        written.len()
    );
    // And they must not all be filed under the directory the test ran in.
    let current = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    let elsewhere = written
        .iter()
        .filter(|cwd| !cwd.replace('\\', "/").to_lowercase().ends_with(&current))
        .count();
    assert!(
        elsewhere > 0 || written.len() == 1,
        "every one of {} sessions landed in the current project",
        written.len()
    );
}
