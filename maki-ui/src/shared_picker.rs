//! The shared-session picker, drawn once before the attached UI opens.
//!
//! A terminal that attaches without a link picks from what the local network
//! announces. Not an `App` overlay: it runs before any session exists, so the
//! choice is made first and only then is a connection opened. It renders in
//! normal flow, so the UI that follows opens over it and a cancelled pick stays
//! in scrollback.

use std::io::{self, stdout};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

use crate::components::{hint_line, is_ctrl};
use crate::theme;

const TITLE: &str = " Shared sessions ";
const HINTS: &[(&str, &str)] = &[("↑/↓", "Select"), ("Enter", "Attach"), ("Esc", "Cancel")];
const SELECTED_MARKER: &str = "> ";
const UNSELECTED_MARKER: &str = "  ";
const INDENT: &str = "  ";
const ADDRESS_SEPARATOR: &str = " · ";
const BORDER_LINES: u16 = 2;
const FALLBACK_WIDTH: u16 = 80;
const FALLBACK_HEIGHT: u16 = 24;

/// One announced session, as the picker shows it: the name to read and where
/// it answers.
pub struct SharedSession<'a> {
    pub label: &'a str,
    pub address: &'a str,
}

/// The row the user chose, or `None` when they cancelled. A drawing failure is
/// a cancel too: attaching to a session nobody confirmed is worse than asking
/// again next time.
pub fn pick_shared_session(sessions: &[SharedSession<'_>]) -> Option<usize> {
    if sessions.is_empty() {
        return None;
    }
    let mut picker = Picker::new(sessions.len());
    match draw_and_read(&mut picker, sessions) {
        Ok(choice) => choice,
        Err(error) => {
            tracing::warn!(%error, "could not draw the shared-session picker");
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    Chosen(usize),
    Cancelled,
}

struct Picker {
    selected: usize,
    count: usize,
}

impl Picker {
    fn new(count: usize) -> Self {
        Self { selected: 0, count }
    }

    /// Pure, so the key handling is testable without a terminal. Ctrl-C is
    /// deliberately not an answer; the caller turns it into a cancel.
    fn handle_key(&mut self, key: KeyEvent) -> Option<Answer> {
        match key.code {
            KeyCode::Enter => Some(Answer::Chosen(self.selected)),
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => Some(Answer::Cancelled),
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                self.selected = (self.selected + self.count - 1) % self.count;
                None
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % self.count;
                None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected = 0;
                None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.count - 1;
                None
            }
            _ => None,
        }
    }

    fn lines(&self, sessions: &[SharedSession<'_>]) -> Vec<Line<'static>> {
        let theme = theme::current();
        let mut lines = vec![Line::raw("")];
        lines.extend(sessions.iter().enumerate().map(|(index, session)| {
            let selected = index == self.selected;
            let marker = if selected {
                SELECTED_MARKER
            } else {
                UNSELECTED_MARKER
            };
            let label = if selected {
                theme.item_selected
            } else {
                theme.item
            };
            Line::from(vec![
                Span::raw(INDENT),
                Span::styled(marker, label),
                Span::styled(session.label.to_owned(), label),
                Span::styled(
                    format!("{ADDRESS_SEPARATOR}{}", session.address),
                    theme.item_desc,
                ),
            ])
        }));
        lines.push(Line::raw(""));
        lines.push(hint_line(HINTS));
        lines
    }

    fn height(&self, sessions: &[SharedSession<'_>], width: u16) -> u16 {
        let wrapped = Paragraph::new(self.lines(sessions))
            .wrap(Wrap { trim: false })
            .line_count(width.saturating_sub(BORDER_LINES)) as u16;
        (wrapped + BORDER_LINES).min(screen_height())
    }

    fn view(&self, frame: &mut Frame, area: Rect, sessions: &[SharedSession<'_>]) {
        let theme = theme::current();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme.panel_border)
            .title(TITLE)
            .title_style(theme.panel_title)
            .style(Style::new().bg(theme.background));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(self.lines(sessions)).wrap(Wrap { trim: false }),
            inner,
        );
    }
}

fn screen_height() -> u16 {
    terminal::size().map_or(FALLBACK_HEIGHT, |(_, height)| height)
}

fn draw_and_read(picker: &mut Picker, sessions: &[SharedSession<'_>]) -> io::Result<Option<usize>> {
    let width = terminal::size().map_or(FALLBACK_WIDTH, |(width, _)| width);
    let height = picker.height(sessions, width);
    terminal::enable_raw_mode()?;
    let result = run(picker, sessions, height);
    terminal::disable_raw_mode().ok();
    result
}

fn run(
    picker: &mut Picker,
    sessions: &[SharedSession<'_>],
    height: u16,
) -> io::Result<Option<usize>> {
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    terminal.hide_cursor()?;

    let answer = loop {
        terminal.draw(|frame| picker.view(frame, frame.area(), sessions))?;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if is_ctrl(&key) && matches!(key.code, KeyCode::Char('c' | 'C')) {
                    break Answer::Cancelled;
                }
                if let Some(answer) = picker.handle_key(key) {
                    break answer;
                }
            }
            Event::Resize(..) => terminal.autoresize()?,
            _ => {}
        }
    };

    terminal.draw(|frame| picker.view(frame, frame.area(), sessions))?;
    terminal.show_cursor()?;
    // Leaves the pick above the cursor, so the session that follows starts on a
    // fresh line instead of over it.
    terminal.backend_mut().append_lines(height)?;
    Ok(match answer {
        Answer::Chosen(index) => Some(index),
        Answer::Cancelled => None,
    })
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    use super::*;

    const ROWS: usize = 3;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn picker() -> Picker {
        Picker::new(ROWS)
    }

    #[test_case(KeyCode::Down, 1 ; "down_moves")]
    #[test_case(KeyCode::Tab, 1 ; "tab_moves")]
    #[test_case(KeyCode::Char('j'), 1 ; "j_moves")]
    fn a_key_moves_to_a_row(code: KeyCode, expected: usize) {
        let mut picker = picker();
        assert_eq!(picker.handle_key(press(code)), None);
        assert_eq!(
            picker.handle_key(press(KeyCode::Enter)),
            Some(Answer::Chosen(expected))
        );
    }

    #[test]
    fn enter_takes_the_first_row_until_something_moves() {
        assert_eq!(
            picker().handle_key(press(KeyCode::Enter)),
            Some(Answer::Chosen(0))
        );
    }

    #[test_case(KeyCode::Esc ; "esc_cancels")]
    #[test_case(KeyCode::Char('q') ; "q_cancels")]
    fn leaving_answers_nothing(code: KeyCode) {
        assert_eq!(picker().handle_key(press(code)), Some(Answer::Cancelled));
    }

    #[test]
    fn movement_wraps_at_both_ends() {
        let mut picker = picker();

        assert_eq!(picker.handle_key(press(KeyCode::Up)), None);
        assert_eq!(picker.selected, ROWS - 1);

        assert_eq!(picker.handle_key(press(KeyCode::Down)), None);
        assert_eq!(picker.selected, 0);
    }

    #[test_case(KeyCode::End, ROWS - 1 ; "end")]
    #[test_case(KeyCode::Char('G'), ROWS - 1 ; "shift_g")]
    fn jumping_reaches_the_last_row(code: KeyCode, expected: usize) {
        let mut picker = picker();
        assert_eq!(picker.handle_key(press(code)), None);
        assert_eq!(picker.selected, expected);
        assert_eq!(picker.handle_key(press(KeyCode::Home)), None);
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn ctrl_c_is_not_an_answer() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(picker().handle_key(key), None);
    }

    #[test]
    fn every_session_gets_a_row() {
        let sessions = [
            SharedSession {
                label: "makima",
                address: "10.0.0.2:7420",
            },
            SharedSession {
                label: "csharp-format",
                address: "10.0.0.3:7420",
            },
        ];
        let mut picker = Picker::new(sessions.len());
        picker.selected = 1;

        let rows: Vec<String> = picker
            .lines(&sessions)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert!(rows[1].contains("makima"));
        assert!(rows[1].contains("10.0.0.2:7420"));
        assert!(rows[2].starts_with(&format!("{INDENT}{SELECTED_MARKER}csharp-format")));
    }

    #[test]
    fn the_card_draws_every_session_under_its_title() {
        let sessions = [
            SharedSession {
                label: "makima",
                address: "10.0.0.2:7420",
            },
            SharedSession {
                label: "csharp-format",
                address: "10.0.0.3:7420",
            },
        ];
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal
            .draw(|frame| Picker::new(sessions.len()).view(frame, frame.area(), &sessions))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        assert!(screen.contains(TITLE.trim()));
        assert!(screen.contains("makima"));
        assert!(screen.contains("10.0.0.3:7420"));
        assert!(screen.contains("Attach"));
    }
}
