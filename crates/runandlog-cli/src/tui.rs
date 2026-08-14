//! Terminal UI. Pick a cell, run it, and read the result in place.

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use runandlog_core::ExecOutcome;

use crate::session::Session;

// The run/draw loop needs a terminal and cannot be covered by automated tests, so
// the tests live in runandlog-core (parse, exec, render) and in session (write-back).

/// Interval between input polls and redraws.
const TICK: Duration = Duration::from_millis(100);
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];
const PAGE_LINES: usize = 10;

/// Opens the TUI.
pub fn run(session: Session) -> io::Result<()> {
    let terminal = ratatui::init();
    let result = App::new(session).run(terminal);
    ratatui::restore();
    result
}

/// Whether a "run all" batch may go on to the next cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Batch {
    Continue,
    Stop,
}

/// Whether a key press means "quit".
fn is_quit_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => true,
        KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

/// The lines to draw, plus the line range occupied by each cell.
struct Rendered {
    lines: Vec<Line<'static>>,
    /// (first line, one past the last line) for each cell.
    spans: Vec<(usize, usize)>,
}

struct App {
    session: Session,
    selected: usize,
    scroll: usize,
    /// Height of the body area. Used to adjust the scroll position.
    viewport: usize,
    status: String,
    /// The cell being run (0-based) and the spinner phase.
    running: Option<(usize, usize)>,
    quit: bool,
}

impl App {
    fn new(session: Session) -> App {
        let status = if session.is_empty() {
            "No runnable cells. Press q to quit.".to_string()
        } else {
            "Enter/r: run  a: run all  j/k: move  R: reload  q: quit".to_string()
        };
        App {
            session,
            selected: 0,
            scroll: 0,
            viewport: 1,
            status,
            running: None,
            quit: false,
        }
    }

    fn run(mut self, mut terminal: DefaultTerminal) -> io::Result<()> {
        while !self.quit {
            self.redraw(&mut terminal)?;
            self.handle_events(&mut terminal)?;
        }
        Ok(())
    }

    fn redraw(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let rendered = self.render_lines();
        terminal.draw(|frame| {
            let areas = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(frame.area());

            let header = Line::from(vec![
                Span::styled("File: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.session.path().display().to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]);
            frame.render_widget(Paragraph::new(header), areas[0]);

            // The body height is what is left after the top and bottom borders.
            self.viewport = (areas[1].height.saturating_sub(2) as usize).max(1);
            self.adjust_scroll(&rendered);

            let body = Paragraph::new(Text::from(rendered.lines.clone()))
                .block(Block::default().borders(Borders::TOP | Borders::BOTTOM))
                .scroll((self.scroll as u16, 0));
            frame.render_widget(body, areas[1]);

            let status = match self.running {
                Some((index, phase)) => format!(
                    "{} running cell {}",
                    SPINNER[phase % SPINNER.len()],
                    index + 1
                ),
                None => self.status.clone(),
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    status,
                    Style::default().fg(Color::DarkGray),
                ))),
                areas[2],
            );
        })?;
        Ok(())
    }

    /// Builds the lines to display.
    fn render_lines(&self) -> Rendered {
        let mut lines = Vec::new();
        let mut spans = Vec::new();
        let doc = self.session.doc();

        for cell in &doc.cells {
            let start = lines.len();
            let selected = cell.index == self.selected;
            let marker_style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" [{}] > Run ", cell.display_number()), marker_style),
                Span::styled(
                    match &cell.out_file {
                        Some(path) => format!("  -> {path}"),
                        None => String::new(),
                    },
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            for command in cell.command.lines() {
                lines.push(Line::from(vec![
                    Span::styled("   > ", Style::default().fg(Color::DarkGray)),
                    Span::raw(command.to_string()),
                ]));
            }
            if let Some(result) = doc.result_text(cell) {
                lines.push(Line::from(""));
                for line in result.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("   {line}"),
                        Style::default().fg(Color::Gray),
                    )));
                }
            }
            lines.push(Line::from(""));
            spans.push((start, lines.len()));
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No shell / sh / bash / zsh code block found.",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Rendered { lines, spans }
    }

    /// Nudges the scroll position so the selected cell stays on screen.
    fn adjust_scroll(&mut self, rendered: &Rendered) {
        if let Some(&(start, end)) = rendered.spans.get(self.selected) {
            if start < self.scroll {
                self.scroll = start;
            } else if end > self.scroll + self.viewport {
                // For a cell taller than the viewport, prefer its top over its bottom.
                self.scroll = end.saturating_sub(self.viewport).min(start);
            }
        }
        self.scroll = self.scroll.min(rendered.lines.len().saturating_sub(1));
    }

    fn handle_events(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        if !event::poll(TICK)? {
            return Ok(());
        }
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            self.handle_key(key, terminal)?;
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) -> io::Result<()> {
        if is_quit_key(key) {
            self.quit = true;
            return Ok(());
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.select(1),
            KeyCode::Char('k') | KeyCode::Up => self.select(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = 0;
                self.scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.session.len().saturating_sub(1);
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(PAGE_LINES),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(PAGE_LINES),
            KeyCode::Char('R') => self.reload(),
            KeyCode::Enter | KeyCode::Char('r') => {
                if !self.session.is_empty() {
                    self.execute(self.selected, terminal)?;
                }
            }
            KeyCode::Char('a') => {
                for index in 0..self.session.len() {
                    if self.quit {
                        break;
                    }
                    self.selected = index;
                    if self.execute(index, terminal)? == Batch::Stop {
                        break;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn select(&mut self, delta: isize) {
        if self.session.is_empty() {
            return;
        }
        let last = self.session.len() - 1;
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    fn reload(&mut self) {
        match self.session.reload() {
            Ok(()) => {
                self.selected = self.selected.min(self.session.len().saturating_sub(1));
                self.status = "Reloaded.".to_string();
            }
            Err(error) => self.status = format!("Reload failed: {error}"),
        }
    }

    /// Runs one cell. The run goes to a worker thread so the screen keeps
    /// updating while it is in flight.
    ///
    /// The return value tells a "run all" batch whether it may continue.
    fn execute(&mut self, index: usize, terminal: &mut DefaultTerminal) -> io::Result<Batch> {
        let command = self.session.command_of(index);
        let options = self.session.exec_options();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(runandlog_core::run(&command, &options));
        });

        let outcome = self.wait_for(index, rx, terminal);
        self.running = None;
        match outcome {
            Ok(Ok(outcome)) => match self.session.apply_outcome(index, &outcome) {
                Ok(()) => {
                    self.status = format!("Cell {} done ({})", index + 1, outcome.status_text());
                }
                Err(error) => {
                    // The write was refused, which usually means the file changed
                    // underneath us. Carrying on would run the *old* commands held in
                    // memory -- including ones the file no longer contains -- so stop
                    // the batch and pick the file back up from disk.
                    self.status = format!("Writing the result failed: {error}");
                    self.reload_after_conflict();
                    return Ok(Batch::Stop);
                }
            },
            Ok(Err(error)) => self.status = format!("The run failed: {error}"),
            Err(error) => return Err(error),
        }
        Ok(Batch::Continue)
    }

    /// Re-reads the file after a refused write, keeping the status text that
    /// explains why the write was refused.
    fn reload_after_conflict(&mut self) {
        if self.session.reload().is_ok() {
            self.selected = self.selected.min(self.session.len().saturating_sub(1));
        }
    }

    /// Handles keys pressed while a command is running.
    ///
    /// A running command cannot be stopped, so only a quit request is honoured
    /// here and everything else is discarded. Without discarding them, keys
    /// pressed during the run would all fire at once once it finishes.
    ///
    /// The quit keys are the same ones the normal event loop takes, so that the
    /// documented "q quits after the command finishes" holds while one is running.
    fn drain_events_while_running(&mut self) -> io::Result<()> {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && is_quit_key(key)
            {
                // Quit after the command finishes; its result still gets written back.
                self.quit = true;
            }
        }
        Ok(())
    }

    /// Waits for the run to finish, spinning the spinner and redrawing meanwhile.
    fn wait_for(
        &mut self,
        index: usize,
        rx: mpsc::Receiver<io::Result<ExecOutcome>>,
        terminal: &mut DefaultTerminal,
    ) -> io::Result<io::Result<ExecOutcome>> {
        let mut phase = 0;
        loop {
            match rx.recv_timeout(TICK) {
                Ok(result) => return Ok(result),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    phase += 1;
                    self.running = Some((index, phase));
                    self.redraw(terminal)?;
                    self.drain_events_while_running()?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Ok(Err(io::Error::other("the worker thread died unexpectedly")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn recognises_the_documented_quit_keys() {
        assert!(is_quit_key(key(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(is_quit_key(key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(is_quit_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn a_plain_c_is_not_a_quit_key() {
        assert!(!is_quit_key(key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(!is_quit_key(key(KeyCode::Char('r'), KeyModifiers::NONE)));
        assert!(!is_quit_key(key(KeyCode::Char('a'), KeyModifiers::NONE)));
    }
}
