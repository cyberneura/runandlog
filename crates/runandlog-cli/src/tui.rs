//! ターミナル UI。セルを選んで実行し、結果をその場で確認する。

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

// 実行と描画の往復は端末が必要で自動テストでは検証できないため、テストは
// runandlog-core (パース・実行・整形) と session (書き戻し) 側に置いている。

/// 入力待ちと画面更新の間隔。
const TICK: Duration = Duration::from_millis(100);
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];
const PAGE_LINES: usize = 10;

/// TUI を開く。
pub fn run(session: Session) -> io::Result<()> {
    let terminal = ratatui::init();
    let result = App::new(session).run(terminal);
    ratatui::restore();
    result
}

/// 画面に描く行と、セルごとの行範囲。
struct Rendered {
    lines: Vec<Line<'static>>,
    /// セルごとの (開始行, 終端行)。終端は含まない。
    spans: Vec<(usize, usize)>,
}

struct App {
    session: Session,
    selected: usize,
    scroll: usize,
    /// 本文領域の高さ。スクロール位置の調整に使う。
    viewport: usize,
    status: String,
    /// 実行中のセル (0 始まり) とスピナーの位相。
    running: Option<(usize, usize)>,
    quit: bool,
}

impl App {
    fn new(session: Session) -> App {
        let status = if session.is_empty() {
            "実行できるセルがありません。q で終了します。".to_string()
        } else {
            "Enter/r: 実行  a: 全実行  j/k: 移動  R: 再読込  q: 終了".to_string()
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

            // 上下のボーダー 2 行を除いた分が本文の高さ。
            self.viewport = (areas[1].height.saturating_sub(2) as usize).max(1);
            self.adjust_scroll(&rendered);

            let body = Paragraph::new(Text::from(rendered.lines.clone()))
                .block(Block::default().borders(Borders::TOP | Borders::BOTTOM))
                .scroll((self.scroll as u16, 0));
            frame.render_widget(body, areas[1]);

            let status = match self.running {
                Some((index, phase)) => format!(
                    "{} 実行中: セル {}",
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

    /// 画面に出す行を組み立てる。
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
                "  shell / sh / bash / zsh のコードブロックが見つかりません。",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Rendered { lines, spans }
    }

    /// 選択中のセルが画面外に出ないようにスクロール位置を寄せる。
    fn adjust_scroll(&mut self, rendered: &Rendered) {
        if let Some(&(start, end)) = rendered.spans.get(self.selected) {
            if start < self.scroll {
                self.scroll = start;
            } else if end > self.scroll + self.viewport {
                // セルが画面より高い場合は末尾ではなく先頭を優先する。
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
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
                    self.execute(index, terminal)?;
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
                self.status = "読み直しました。".to_string();
            }
            Err(error) => self.status = format!("読み直しに失敗しました: {error}"),
        }
    }

    /// 1 セルを実行する。実行はワーカースレッドに投げ、待っている間も画面を更新する。
    fn execute(&mut self, index: usize, terminal: &mut DefaultTerminal) -> io::Result<()> {
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
                    self.status = format!("セル {} 完了 ({})", index + 1, outcome.status_text());
                }
                Err(error) => self.status = format!("結果の書き込みに失敗しました: {error}"),
            },
            Ok(Err(error)) => self.status = format!("実行に失敗しました: {error}"),
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// 実行中に押されたキーを処理する。
    ///
    /// 実行中のコマンドは止められないので、ここで受け付けるのは終了要求だけ。残りは捨てる。
    /// 捨てないと、実行中に押したキーが完了後にまとめて効いてしまう。
    fn drain_events_while_running(&mut self) -> io::Result<()> {
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                // コマンドの完了を待ってから終了する (結果は書き戻される)。
                self.quit = true;
            }
        }
        Ok(())
    }

    /// 実行の完了を待つ。待っている間はスピナーを回して画面の更新を続ける。
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
                    return Ok(Err(io::Error::other("実行スレッドが異常終了しました")));
                }
            }
        }
    }
}
