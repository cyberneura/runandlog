//! Entry point of the Run and Log CLI / TUI.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use runandlog_cli::session::Session;
use runandlog_cli::tui;
use runandlog_core::ExecOptions;

/// Runs the shell commands written in a Markdown file and writes the results
/// back into the same file.
#[derive(Parser, Debug)]
#[command(name = "runandlog", version, about, long_about = None)]
struct Args {
    /// The Markdown file to work on.
    file: PathBuf,

    /// Open in the desktop app (GUI).
    #[arg(short, long)]
    gui: bool,

    /// Print the list of cells and exit.
    #[arg(short, long)]
    list: bool,

    /// Run only the cell with this number (1-based). May be repeated.
    #[arg(short, long, value_name = "N")]
    run: Vec<usize>,

    /// Run every cell in order.
    #[arg(short = 'a', long)]
    run_all: bool,

    /// Write output to a separate file once it exceeds this many lines.
    #[arg(long, value_name = "N", default_value_t = 50)]
    max_inline_lines: usize,

    /// Shell the commands are handed to. Defaults to the SHELL environment variable.
    #[arg(long, value_name = "PATH")]
    shell: Option<PathBuf>,

    /// Working directory for the commands. Defaults to the directory of the Markdown file.
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// Time limit per cell, in seconds. Unlimited by default.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match dispatch(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("runandlog: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: Args) -> std::io::Result<ExitCode> {
    if args.gui {
        eprintln!("runandlog: the GUI is not implemented yet. Drop --gui to open the TUI.");
        return Ok(ExitCode::from(2));
    }

    let mut session = Session::load(&args.file, exec_options(&args), args.max_inline_lines)?;

    if args.list {
        print_list(&session);
        return Ok(ExitCode::SUCCESS);
    }

    let targets = targets(&args, &session)?;
    if targets.is_empty() {
        tui::run(session)?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut failed = false;
    for index in targets {
        let cell = &session.doc().cells[index];
        println!("[{}] {}", index + 1, first_line(&cell.command));
        let outcome = session.run_cell(index)?;
        print!("{}", outcome.output);
        println!("--- {}", outcome.status_text());
        failed |= !outcome.is_success();
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn exec_options(args: &Args) -> ExecOptions {
    let cwd = args.cwd.clone().unwrap_or_else(|| {
        args.file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    });
    let mut options = ExecOptions::new(cwd);
    if let Some(shell) = &args.shell {
        options.shell = shell.clone();
    }
    options.timeout = args.timeout.map(Duration::from_secs);
    options
}

/// Decides which cells to run, as 0-based indices. An empty result opens the TUI.
fn targets(args: &Args, session: &Session) -> std::io::Result<Vec<usize>> {
    if args.run_all {
        return Ok((0..session.len()).collect());
    }
    let mut targets = Vec::new();
    for number in &args.run {
        if *number == 0 || *number > session.len() {
            return Err(std::io::Error::other(format!(
                "no such cell: {number} (the file has {} cells)",
                session.len()
            )));
        }
        targets.push(number - 1);
    }
    Ok(targets)
}

fn print_list(session: &Session) {
    if session.is_empty() {
        println!("no runnable cells in {}", session.path().display());
        return;
    }
    for cell in &session.doc().cells {
        let out = match &cell.out_file {
            Some(path) => format!(" -> {path}"),
            None => String::new(),
        };
        println!(
            "[{}] {}{out}",
            cell.display_number(),
            first_line(&cell.command)
        );
    }
}

fn first_line(command: &str) -> String {
    let mut lines = command.lines().filter(|line| !line.trim().is_empty());
    let first = lines.next().unwrap_or("").trim().to_string();
    if lines.next().is_some() {
        return format!("{first} ...");
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_ellipsis_for_multi_line_commands() {
        assert_eq!(first_line("ls /opt\nls /tmp\n"), "ls /opt ...");
        assert_eq!(first_line("date\n"), "date");
        assert_eq!(first_line("\n\n"), "");
    }
}
