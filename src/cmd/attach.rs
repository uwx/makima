//! Driving an agent another machine serves, from a terminal. The full
//! interface attaches to it like any other session; `--plain` keeps the line
//! renderer, which is also what a pipe gets.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use maki_acp_client::{
    Connection, ContentBlock, Event, PermissionOption, PermissionPrompt, PickOutcome, PickPrompt,
    SessionUpdate, StopReason, Target, ToolCallStatus,
};
use maki_lan::discovery::{Browser, Discovered, Found};
use maki_storage::StateDir;
use maki_ui::{SharedSession, pick_shared_session};

use crate::cli::Cli;
use crate::cmd::tui;

const CANCEL_COMMAND: &str = "cancel";
const QUIT_COMMAND: &str = "quit";
const HELP_COMMAND: &str = "help";
const COMMAND_PREFIX: char = '/';
const PROMPT_MARKER: &str = "> ";
const ANSWER_MARKER: &str = "  ";
const HELP: &str = "/cancel stops a turn, /quit leaves, /help repeats this";
const AGENT_GONE: &str = "the agent went away";
const PERMISSION_QUESTION: &str = "Allow? Enter denies";
const PICK_QUESTION: &str = "Choose? Enter closes";
const CHOOSE_QUESTION: &str = "Which one? Enter cancels";
const TOKEN_QUESTION: &str = "Token (Enter uses the one on this machine)";
const ANNOUNCED_HEADER: &str = "shared sessions on the local network:";
const NOTHING_ANNOUNCED: &str =
    "no shared session was announced; pass a link, like `makima attach 192.168.2.4:60667`";
const NOTHING_CHOSEN: &str = "nothing chosen";
const CWD_ERR: &str = "cannot read the current directory (was it deleted?)";
/// How long a terminal listens for announcements before showing what it has.
const ANNOUNCE_WAIT: Duration = Duration::from_secs(3);

/// A line renderer answers a prompt and exits, and a pipe has no screen to
/// draw: both stay on the terminal path, everything else gets the full UI.
pub fn run(
    cli: Cli,
    link: Option<String>,
    token: Option<String>,
    plain: bool,
    prompt: Option<String>,
) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    let interactive = !plain && prompt.is_none() && io::stdout().is_terminal();
    let (link, token) = match link {
        Some(link) => (link, token),
        None => match choose(&token, interactive)? {
            Some(chosen) => chosen,
            None => return Ok(()),
        },
    };
    let token = token_for(token, &storage)?;
    let target = Target::lan(&with_scheme(&link), Some(&token)).map_err(|error| eyre!(error))?;
    let connection = maki_acp_client::connect(&target).map_err(|error| eyre!(error))?;
    if interactive {
        let label = target.label().to_owned();
        let cwd = std::env::current_dir().context(CWD_ERR)?;
        return tui::run(
            cli,
            cwd,
            Some(maki_ui::RemoteSession {
                connection,
                label,
                owned: false,
            }),
        );
    }
    let mut session = Session::new(connection);
    match prompt {
        Some(text) => session.turn(&text),
        None => session.interactive(),
    }
}

/// Which announced session to attach to, and the token it expects. A terminal
/// started without a link picks from what the local network offers.
fn choose(given: &Option<String>, interactive: bool) -> Result<Option<(String, Option<String>)>> {
    let found = announced();
    if found.is_empty() {
        bail!(NOTHING_ANNOUNCED);
    }
    let Some(session) = pick(&found, interactive)? else {
        return Ok(None);
    };
    let token = match given {
        Some(token) => Some(token.clone()),
        None => ask_token()?,
    };
    Ok(Some((maki_lan::link(session.address), token)))
}

/// The announced session the user names: a full-screen pick on a terminal that
/// can draw one, a typed number when there is no screen to draw on.
fn pick(found: &[Found], interactive: bool) -> Result<Option<&Found>> {
    if interactive {
        let addresses: Vec<String> = found
            .iter()
            .map(|session| session.address.to_string())
            .collect();
        let rows: Vec<SharedSession<'_>> = found
            .iter()
            .zip(&addresses)
            .map(|(session, address)| SharedSession {
                label: label(session),
                address,
            })
            .collect();
        return Ok(pick_shared_session(&rows).and_then(|index| found.get(index)));
    }
    let mut printer = Printer::new();
    printer.line(ANNOUNCED_HEADER);
    for (index, session) in found.iter().enumerate() {
        printer.line(&format!(
            "  {}. {} · {}",
            index + 1,
            label(session),
            session.address
        ));
    }
    let Some(answer) = read_answer(&mut printer, CHOOSE_QUESTION)? else {
        return Ok(None);
    };
    let chosen = picked(&answer, found.len()).and_then(|index| found.get(index));
    if chosen.is_none() {
        printer.line(NOTHING_CHOSEN);
    }
    Ok(chosen)
}

/// What the local network announces, waiting long enough to hear back.
fn announced() -> Vec<Found> {
    let Some(browser) = Browser::start() else {
        return Vec::new();
    };
    let deadline = Instant::now() + ANNOUNCE_WAIT;
    let mut found: Vec<Found> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match browser.next(left) {
            Some(Discovered::Found(session)) => {
                if !found.iter().any(|known| known.fullname == session.fullname) {
                    found.push(session);
                }
            }
            Some(Discovered::Gone(fullname)) => {
                found.retain(|known| known.fullname != fullname);
            }
            // Nothing was announced before the deadline, or browsing stopped.
            None => break,
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// The name to show for an announced session: its directory, or what it calls
/// itself when it says nothing about one.
fn label(found: &Found) -> &str {
    if found.project.is_empty() {
        &found.name
    } else {
        &found.project
    }
}

/// The index a typed number names, counting from one.
fn picked(answer: &str, count: usize) -> Option<usize> {
    let index = answer.trim().parse::<usize>().ok()?.checked_sub(1)?;
    (index < count).then_some(index)
}

/// Asks for the token the chosen agent expects, where an empty answer keeps the
/// one this machine already has.
fn ask_token() -> Result<Option<String>> {
    let mut printer = Printer::new();
    let token = read_answer(&mut printer, TOKEN_QUESTION)?
        .unwrap_or_default()
        .trim()
        .to_owned();
    Ok((!token.is_empty()).then_some(token))
}

/// The token the agent expects: the flag, then the environment, then the one
/// stored for this machine.
fn token_for(flag: Option<String>, storage: &StateDir) -> Result<String> {
    if let Some(token) = flag {
        return Ok(token);
    }
    maki_lan::token(storage.path()).context("read the LAN token")
}

/// A pasted `host:port` reaches the same place as a full link.
fn with_scheme(link: &str) -> String {
    if link.starts_with(maki_lan::LINK_SCHEME) {
        link.to_owned()
    } else {
        format!("{}{link}", maki_lan::LINK_SCHEME)
    }
}

enum Command {
    Empty,
    Help,
    Quit,
    Cancel,
    Prompt(String),
}

/// A line starting with a slash names what this terminal does; anything else is
/// a prompt for the agent.
fn command(line: &str) -> Command {
    let text = line.trim();
    if text.is_empty() {
        return Command::Empty;
    }
    let Some(name) = text.strip_prefix(COMMAND_PREFIX) else {
        return Command::Prompt(text.to_owned());
    };
    match name.trim() {
        CANCEL_COMMAND => Command::Cancel,
        QUIT_COMMAND => Command::Quit,
        HELP_COMMAND => Command::Help,
        _ => Command::Prompt(text.to_owned()),
    }
}

struct Session {
    connection: Connection,
    printer: Printer,
}

impl Session {
    fn new(connection: Connection) -> Self {
        let mut printer = Printer::new();
        let mut attached = format!(
            "attached to session {}",
            maki_acp_client::short_session(&connection.session)
        );
        if !connection.model.is_empty() {
            attached.push_str(&format!(" on {}", connection.model));
        }
        printer.line(&attached);
        printer.line(HELP);
        Self {
            connection,
            printer,
        }
    }

    fn interactive(&mut self) -> Result<()> {
        loop {
            self.show_pending()?;
            self.printer.prompt(PROMPT_MARKER);
            let Some(line) = read_line()? else {
                self.printer.end_line();
                return Ok(());
            };
            match command(&line) {
                Command::Empty => {}
                Command::Help => self.help(),
                Command::Quit => return Ok(()),
                Command::Cancel => self.connection.client.cancel(),
                Command::Prompt(text) => self.turn(&text)?,
            }
        }
    }

    fn help(&mut self) {
        self.printer.line(HELP);
    }

    /// Shows what the agent sent while this terminal was not running a turn,
    /// which is the replay first and other windows' changes after it.
    fn show_pending(&mut self) -> Result<()> {
        while let Ok(event) = self.connection.events.try_recv() {
            self.on_event(event)?;
        }
        Ok(())
    }

    /// Runs one prompt to its end, answering what the agent asks meanwhile.
    fn turn(&mut self, text: &str) -> Result<()> {
        let client = Arc::clone(&self.connection.client);
        let prompt = text.to_owned();
        let (finished, finish) = flume::bounded(1);
        thread::Builder::new()
            .name("maki-attach-prompt".into())
            .spawn(move || {
                let _ = finished.send(client.prompt(&prompt));
            })
            .context("start the prompt")?;
        let events = self.connection.events.clone();
        loop {
            let step = smol::block_on(smol::future::or(
                async { Step::Event(events.recv_async().await) },
                async { Step::Done(finish.recv_async().await) },
            ));
            match step {
                Step::Event(Ok(event)) => self.on_event(event)?,
                Step::Event(Err(_)) => bail!(AGENT_GONE),
                Step::Done(Ok(Ok(reason))) => {
                    self.printer.end_line();
                    if reason != StopReason::EndTurn {
                        self.printer.line(&format!("stopped: {reason:?}"));
                    }
                    return Ok(());
                }
                Step::Done(Ok(Err(error))) => {
                    self.printer.line(&format!("prompt failed: {error}"));
                    return Ok(());
                }
                Step::Done(Err(_)) => bail!(AGENT_GONE),
            }
        }
    }

    fn on_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Update(update) => self.printer.update(&update),
            Event::Cwd(cwd) => self
                .printer
                .line(&format!("Working directory: {}", cwd.display())),
            Event::Permission(prompt) => self.answer_permission(prompt)?,
            Event::Pick(prompt) => self.answer_pick(prompt)?,
            Event::Flash(text) => self.printer.line(&text),
            Event::Closed(reason) => bail!(reason),
            _ => {}
        }
        Ok(())
    }

    fn answer_permission(&mut self, prompt: PermissionPrompt) -> Result<()> {
        self.printer
            .line(&format!("{} needs permission", prompt.title));
        if let Some(diff) = prompt.diff {
            for line in diff.lines() {
                self.printer.line(&format!("{ANSWER_MARKER}{line}"));
            }
        }
        self.printer.choices(&prompt.options);
        let answer = read_choice(&mut self.printer, PERMISSION_QUESTION)?
            .and_then(|number| prompt.options.get(number))
            .map(|option| option.option_id.0.to_string());
        self.connection.client.answer(prompt.id, answer.as_deref());
        Ok(())
    }

    fn answer_pick(&mut self, prompt: PickPrompt) -> Result<()> {
        self.printer.line(&prompt.title);
        for (label, value) in &prompt.footer {
            self.printer.line(&format!("  {label}: {value}"));
        }
        for (index, item) in prompt.items.iter().enumerate() {
            let detail = item
                .detail
                .as_ref()
                .map(|detail| format!(" · {detail}"))
                .unwrap_or_default();
            self.printer
                .line(&format!("  {}. {}{detail}", index + 1, item.label));
        }
        let choice = read_choice(&mut self.printer, PICK_QUESTION)?;
        self.connection.client.answer_pick(
            prompt.id,
            match choice {
                Some(index) => PickOutcome::Choice(index),
                None => PickOutcome::Close,
            },
        );
        Ok(())
    }
}

enum Step {
    Event(Result<Event, flume::RecvError>),
    Done(Result<Result<StopReason, String>, flume::RecvError>),
}

/// Reads one line of a prompt, or nothing when the input ended.
fn read_line() -> Result<Option<String>> {
    let mut line = String::new();
    if io::stdin()
        .read_line(&mut line)
        .context("read the answer")?
        == 0
    {
        return Ok(None);
    }
    Ok(Some(line))
}

/// Asks {question} and reads the answer, where the input ending means none came.
fn read_answer(printer: &mut Printer, question: &str) -> Result<Option<String>> {
    printer.line(question);
    printer.prompt(PROMPT_MARKER);
    read_line()
}

/// Reads a numbered choice, where an empty answer or anything unparsable means
/// none was made.
fn read_choice(printer: &mut Printer, question: &str) -> Result<Option<usize>> {
    Ok(read_answer(printer, question)?.and_then(|line| {
        line.trim()
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
    }))
}

struct Printer {
    at_line_start: bool,
    titles: HashMap<String, String>,
}

impl Printer {
    fn new() -> Self {
        Self {
            at_line_start: true,
            titles: HashMap::new(),
        }
    }

    /// Writes streamed text, which shares a line with what follows it.
    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        print!("{text}");
        self.at_line_start = text.ends_with('\n');
        self.flush();
    }

    fn line(&mut self, text: &str) {
        self.end_line();
        println!("{text}");
        self.flush();
    }

    fn end_line(&mut self) {
        if !self.at_line_start {
            println!();
            self.at_line_start = true;
        }
    }

    fn prompt(&mut self, marker: &str) {
        self.end_line();
        print!("{marker}");
        self.at_line_start = false;
        self.flush();
    }

    fn choices(&mut self, options: &[PermissionOption]) {
        for (index, option) in options.iter().enumerate() {
            self.line(&format!("  {}. {}", index + 1, option.name));
        }
    }

    fn update(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => self.chunk(&chunk.content),
            SessionUpdate::UserMessageChunk(chunk) => {
                if let ContentBlock::Text(block) = &chunk.content
                    && !block.text.trim().is_empty()
                {
                    self.line(&format!("you> {}", block.text.trim()));
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.titles
                    .insert(call.tool_call_id.0.to_string(), call.title.clone());
                self.line(&format!("→ {}", call.title));
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let title = update.fields.title.clone().unwrap_or_else(|| {
                    self.titles
                        .get(update.tool_call_id.0.as_ref())
                        .cloned()
                        .unwrap_or_default()
                });
                match update.fields.status {
                    Some(ToolCallStatus::Completed) => self.line(&format!("✓ {title}")),
                    Some(ToolCallStatus::Failed) => self.line(&format!("✗ {title}")),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn chunk(&mut self, content: &ContentBlock) {
        if let ContentBlock::Text(block) = content {
            self.text(&block.text);
        }
    }

    fn flush(&self) {
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, command, picked, with_scheme};
    use test_case::test_case;

    fn action(line: &str) -> &'static str {
        match command(line) {
            Command::Empty => "empty",
            Command::Help => "help",
            Command::Quit => "quit",
            Command::Cancel => "cancel",
            Command::Prompt(_) => "prompt",
        }
    }

    #[test_case(" /quit " => "quit"; "surrounding space is ignored")]
    #[test_case("/help" => "help"; "help")]
    #[test_case("/cancel" => "cancel"; "cancel")]
    #[test_case("" => "empty"; "nothing typed")]
    #[test_case("hello" => "prompt"; "a line is a prompt")]
    #[test_case("/unknown" => "prompt"; "an unknown command is sent on")]
    fn a_line_names_what_the_terminal_does(line: &str) -> &'static str {
        action(line)
    }

    #[test]
    fn a_prompt_keeps_the_text_it_was_given() {
        assert!(
            matches!(command("  fix the tests  "), Command::Prompt(text) if text == "fix the tests")
        );
    }

    #[test_case("box:7331" => "lan://box:7331"; "a pasted address")]
    #[test_case("lan://box:7331:/srv/app" => "lan://box:7331:/srv/app"; "a full link is kept")]
    fn a_link_gets_the_scheme_it_needs(link: &str) -> String {
        with_scheme(link)
    }

    #[test_case("1", 2 => Some(0); "the first of two")]
    #[test_case(" 2 ", 2 => Some(1); "surrounding space is ignored")]
    #[test_case("3", 2 => None; "past the end")]
    #[test_case("0", 2 => None; "counting starts at one")]
    #[test_case("", 2 => None; "nothing typed")]
    #[test_case("two", 2 => None; "not a number")]
    fn a_typed_number_names_an_announced_session(answer: &str, count: usize) -> Option<usize> {
        picked(answer, count)
    }
}
