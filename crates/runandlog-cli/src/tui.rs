//! Terminal UI. Pick a cell, run it, and read the result in place.

use std::collections::HashMap;
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
use runandlog_core::{Canceller, ExecOutcome};

use crate::live::LiveOutput;
use crate::session::Session;

// The run/draw loop needs a terminal and cannot be covered by automated tests, so
// the tests live in runandlog-core (parse, exec, render), in session (write-back)
// and in live (the rolling buffer behind the live view). What is left here is
// tested through the small decisions pulled out as free functions.

/// Interval between input polls and redraws.
const TICK: Duration = Duration::from_millis(100);
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];
const PAGE_LINES: usize = 10;
/// How many trailing lines of a running command's output are shown under its cell.
///
/// Enough to see that something is happening and what it is; more would push the
/// cells below off the screen every time a command runs.
const LIVE_TAIL_LINES: usize = 3;
/// How much of a running command's output is kept for the live view.
///
/// Only the last few lines are drawn, so this only has to be comfortably more than
/// they can take up -- a command printing for hours must not grow with its output.
const LIVE_LIMIT: usize = 16 * 1024;

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

/// Whether a key press means "stop the running command".
///
/// Only Ctrl-C. `q` and `Esc` keep their documented meaning of quitting once the
/// command has finished, which is what you want for a command that is merely slow;
/// Ctrl-C is the way out of one that will never finish.
fn is_cancel_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// What the marker of a cell says, telling apart a first run from a repeat.
///
/// Both are the same width so that the highlighted markers line up down the
/// column, whichever cells happen to carry a result.
fn run_label(has_result: bool) -> &'static str {
    if has_result { "Re-run" } else { "Run   " }
}

/// Where a cell stands in *this* session of the TUI.
///
/// A result already in the file is not a run of this session: reopening a document
/// that was run yesterday should not colour every cell as done. What the colours
/// answer is "how far has this batch got", which only holds for runs since the
/// file was opened or last reloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellState {
    /// Not run since the file was opened.
    Waiting,
    /// Running right now.
    Running,
    /// Run and finished successfully.
    Done,
    /// Run and finished with a non-zero exit, a timeout, or a cancellation.
    Failed,
}

/// Where a cell stands, from the index being run and what has finished.
fn cell_state(index: usize, running: Option<usize>, finished: Option<bool>) -> CellState {
    if running == Some(index) {
        return CellState::Running;
    }
    match finished {
        Some(true) => CellState::Done,
        Some(false) => CellState::Failed,
        None => CellState::Waiting,
    }
}

/// Colour of a cell's marker.
///
/// The marker is what the eye follows down a "run all", so the state is carried
/// there in full: waiting, running now, done, or done badly.
fn marker_color(state: CellState) -> Color {
    match state {
        CellState::Waiting => Color::Green,
        CellState::Running => Color::Yellow,
        CellState::Done => Color::Blue,
        CellState::Failed => Color::Red,
    }
}

/// Style of a cell's command text.
///
/// A finished cell is dimmed so that the cells still to come stand out from the
/// ones already dealt with -- the question during a long batch is where it is up
/// to, and that is easier to see from the text than from a marker alone.
fn command_style(state: CellState) -> Style {
    match state {
        CellState::Done | CellState::Failed => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
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
    /// What the command being run has printed so far.
    live: LiveOutput,
    /// Cells run since the file was opened, and whether each one succeeded.
    ///
    /// Keyed by index, and dropped whenever the file is re-read: after a reload the
    /// cell at an index need not be the one that ran.
    finished: HashMap<usize, bool>,
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
            live: LiveOutput::new(LIVE_LIMIT),
            finished: HashMap::new(),
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

        let running = self.running.map(|(index, _)| index);
        for cell in &doc.cells {
            let start = lines.len();
            let selected = cell.index == self.selected;
            let state = cell_state(cell.index, running, self.finished.get(&cell.index).copied());
            let color = marker_color(state);
            let marker_style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(color)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        " [{}] > {} ",
                        cell.display_number(),
                        run_label(doc.result_text(cell).is_some())
                    ),
                    marker_style,
                ),
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
                    Span::styled(command.to_string(), command_style(state)),
                ]));
            }
            if state == CellState::Running {
                // The live tail takes the place of the previous result rather than
                // sitting under it: the two are the same cell's output from
                // different runs, and stacked they read as one long result.
                lines.extend(self.live_lines());
            } else if let Some(result) = doc.result_text(cell) {
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

    /// The live output of the running cell: its last few lines, as they arrive.
    ///
    /// Always the same height, however little has been printed. Growing the block
    /// line by line would shift every cell below it each time the command prints,
    /// and the reason to watch a running command is to read it.
    fn live_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from("")];
        let tail = self.live.tail_lines(LIVE_TAIL_LINES);
        for index in 0..LIVE_TAIL_LINES {
            let line = match tail.get(index) {
                Some(line) => format!("   {line}"),
                None if index == 0 && tail.is_empty() => "   (no output yet)".to_string(),
                None => String::new(),
            };
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines
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
                // The cells are whatever the file now holds, so which ones this
                // session has run no longer means anything: an index that was run
                // may be a different cell now.
                self.finished.clear();
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
        // One per run: a cancelled cell must not leave the next one unable to start.
        let canceller = Canceller::new();
        let worker_canceller = canceller.clone();
        let (tx, rx) = mpsc::channel();
        // The output is carried over its own channel rather than shared behind a
        // lock: the drawing happens on this thread, and a channel keeps the worker
        // from ever waiting on it.
        let (output_tx, output_rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(runandlog_core::run_streaming(
                &command,
                &options,
                &worker_canceller,
                |chunk| {
                    // A closed channel means the front end has stopped listening,
                    // which is not the command's problem.
                    let _ = output_tx.send(chunk.to_string());
                },
            ));
        });

        self.live.clear();
        // Marked as running before the first draw, so the cell shows itself as the
        // one being run rather than looking untouched until the first tick.
        self.running = Some((index, 0));
        let outcome = self.wait_for(index, rx, output_rx, terminal, &canceller);
        self.running = None;
        self.live.clear();
        match outcome {
            Ok(Ok(outcome)) => match self.session.apply_outcome(index, &outcome) {
                Ok(()) => {
                    self.finished.insert(index, outcome.is_success());
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
            Ok(Err(error)) => {
                // The shell never started. Nothing was written back, but the cell was
                // asked for and did not work, which is what its colour says.
                self.finished.insert(index, false);
                self.status = format!("The run failed: {error}");
            }
            Err(error) => return Err(error),
        }
        Ok(Batch::Continue)
    }

    /// Re-reads the file after a refused write, keeping the status text that
    /// explains why the write was refused.
    fn reload_after_conflict(&mut self) {
        if self.session.reload().is_ok() {
            self.selected = self.selected.min(self.session.len().saturating_sub(1));
            // As in `reload`: the indices no longer name the cells that ran.
            self.finished.clear();
        }
    }

    /// Handles keys pressed while a command is running.
    ///
    /// Only quitting and cancelling are honoured here; everything else is
    /// discarded, since keys pressed during the run would otherwise all fire at
    /// once when it finishes.
    ///
    /// The quit keys are the same ones the normal event loop takes, so that the
    /// documented "q quits after the command finishes" holds while one is running.
    /// Ctrl-C does not wait: raw mode means the terminal never turns it into a
    /// signal, and the command sits in a process group of its own anyway, so this
    /// is the only thing that can reach a command that hangs.
    fn drain_events_while_running(&mut self, canceller: &Canceller) -> io::Result<()> {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if is_cancel_key(key) {
                    canceller.cancel();
                    self.quit = true;
                } else if is_quit_key(key) {
                    // Quit after the command finishes; its result still gets written back.
                    self.quit = true;
                }
            }
        }
        Ok(())
    }

    /// Takes whatever the running command has printed since the last look.
    fn collect_output(&mut self, output_rx: &mpsc::Receiver<String>) {
        // Never blocks: the point is to draw what has arrived, not to wait for more.
        while let Ok(chunk) = output_rx.try_recv() {
            self.live.push(&chunk);
        }
    }

    /// Waits for the run to finish, spinning the spinner and redrawing meanwhile.
    fn wait_for(
        &mut self,
        index: usize,
        rx: mpsc::Receiver<io::Result<ExecOutcome>>,
        output_rx: mpsc::Receiver<String>,
        terminal: &mut DefaultTerminal,
        canceller: &Canceller,
    ) -> io::Result<io::Result<ExecOutcome>> {
        let mut phase = 0;
        loop {
            match rx.recv_timeout(TICK) {
                Ok(result) => {
                    // Drain here too. A cell that finishes inside one TICK would
                    // otherwise return without ever looking at the terminal, so during
                    // "run all" over fast cells a queued q / Esc / Ctrl-C would not be
                    // seen until the whole batch had run.
                    self.drain_events_while_running(canceller)?;
                    return Ok(result);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    phase += 1;
                    self.running = Some((index, phase));
                    self.collect_output(&output_rx);
                    self.redraw(terminal)?;
                    self.drain_events_while_running(canceller)?;
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
    fn a_cell_that_already_ran_says_so() {
        assert_eq!(run_label(true).trim(), "Re-run");
        assert_eq!(run_label(false).trim(), "Run");
        // Equal widths keep the markers aligned down the column.
        assert_eq!(run_label(true).len(), run_label(false).len());
    }

    #[test]
    fn a_cell_that_has_not_run_in_this_session_looks_untouched() {
        // A result from an earlier session is not a run of this one: reopening a
        // document that was run yesterday must not colour every cell as done.
        assert_eq!(cell_state(0, None, None), CellState::Waiting);
    }

    #[test]
    fn the_cell_being_run_is_told_apart_from_the_ones_already_run() {
        assert_eq!(cell_state(1, Some(1), None), CellState::Running);
        assert_eq!(cell_state(1, Some(1), Some(true)), CellState::Running);
        assert_eq!(cell_state(0, Some(1), Some(true)), CellState::Done);
        assert_eq!(cell_state(0, Some(1), Some(false)), CellState::Failed);
    }

    #[test]
    fn every_state_has_a_colour_of_its_own() {
        // The colour is the whole signal, so two states sharing one would make them
        // indistinguishable on screen.
        let colors = [
            marker_color(CellState::Waiting),
            marker_color(CellState::Running),
            marker_color(CellState::Done),
            marker_color(CellState::Failed),
        ];
        for (index, color) in colors.iter().enumerate() {
            assert!(
                !colors[index + 1..].contains(color),
                "two states share {color:?}"
            );
        }
    }

    #[test]
    fn a_finished_cell_is_dimmed_and_the_rest_are_not() {
        assert_eq!(command_style(CellState::Waiting), Style::default());
        assert_eq!(command_style(CellState::Running), Style::default());
        assert_ne!(command_style(CellState::Done), Style::default());
        assert_ne!(command_style(CellState::Failed), Style::default());
    }

    #[test]
    fn a_plain_c_is_not_a_quit_key() {
        assert!(!is_quit_key(key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(!is_quit_key(key(KeyCode::Char('r'), KeyModifiers::NONE)));
        assert!(!is_quit_key(key(KeyCode::Char('a'), KeyModifiers::NONE)));
    }
}
