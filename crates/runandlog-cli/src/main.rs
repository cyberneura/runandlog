//! Entry point of the Run and Log CLI / TUI.

use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use clap::Parser;
use runandlog::session::Session;
use runandlog::tui;
use runandlog_core::{Canceller, ExecOptions};

/// Exit code for a run stopped by a signal, following the shell convention of
/// 128 + the signal number: 130 for SIGINT (Ctrl-C), 143 for SIGTERM.
fn signal_exit_code(signal: i32) -> u8 {
    (128 + signal) as u8
}

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
            // A run that was interrupted still exits by its signal, even when what
            // ended it was the write-back failing rather than the loop finishing.
            // Reporting a plain failure would hide the interruption from a job
            // runner reading the code.
            if interrupt_requested() {
                ExitCode::from(interrupt_exit_code())
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn dispatch(args: Args) -> std::io::Result<ExitCode> {
    let mut session = Session::load(&args.file, exec_options(&args), args.max_inline_lines)?;

    if args.gui {
        // The GUI is checked before the other flags on purpose: --gui is a request
        // for a window, and quietly doing something else instead would be worse
        // than refusing.
        return open_gui(session).map(|()| ExitCode::SUCCESS);
    }

    if args.list {
        print_list(&session);
        return Ok(ExitCode::SUCCESS);
    }

    // Whether to run non-interactively is decided by the flags, not by how many
    // targets they happen to select. `--run-all` on a file with no cells is a
    // request to run nothing, not a request for the TUI -- opening it would hang a
    // script that has no terminal.
    if !args.run_all && args.run.is_empty() {
        tui::run(session)?;
        return Ok(ExitCode::SUCCESS);
    }
    let targets = targets(&args, &session)?;

    let canceller = Canceller::new();
    catch_interrupts(&canceller);

    let mut failed = false;
    for index in targets {
        // Asked before the cell starts as well as after it ends. A signal that lands
        // while the previous result is being written back would otherwise start this
        // cell only for the cancellation to kill it moments later -- and its empty
        // "cancelled" result would replace the result this cell already had, for a
        // run nobody asked for.
        //
        // What is checked is the flag the handler itself sets, not the canceller: the
        // thread that turns one into the other sleeps between looks, so for up to a
        // poll interval after the signal the canceller still says nothing happened.
        if interrupt_requested() {
            break;
        }
        let cell = &session.doc().cells[index];
        println!("[{}] {}", index + 1, first_line(&cell.command));
        // Printed as it arrives rather than once the command has finished. A run
        // that takes minutes otherwise looks like a hung terminal, and the output
        // that would have said what it is doing only appears once it no longer
        // matters. The chunks add up to `outcome.output`, so nothing is printed
        // twice by not printing that afterwards.
        let outcome = session.run_cell_streaming(index, &canceller, print_as_it_arrives)?;
        println!("--- {}", outcome.status_text());
        failed |= !outcome.is_success();
        if interrupt_requested() {
            // The result of the cell that was interrupted has been written back;
            // going on to the next one is not what Ctrl-C asked for.
            break;
        }
    }
    // Asked once more rather than remembered from the loop: a signal landing after
    // the last cell still stopped this run, and reporting success for it would tell
    // a job runner the opposite of what happened.
    if interrupt_requested() {
        eprintln!("runandlog: interrupted");
        return Ok(ExitCode::from(interrupt_exit_code()));
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Prints a piece of a running command's output straight away.
///
/// Flushed by hand: stdout is only line-buffered when it is a terminal, and even
/// then a command printing a prompt or a progress line without a newline would sit
/// in the buffer -- which is the very output a live view exists to show. Redirected
/// to a file it is block-buffered, and nothing would appear until the block filled.
fn print_as_it_arrives(chunk: &str) {
    use std::io::Write;

    print!("{chunk}");
    // A failure here is a closed or full stdout, which the next `println!` reports
    // in the ordinary way. Nothing useful can be done about it mid-command.
    let _ = std::io::stdout().flush();
}

/// Turns Ctrl-C into a request to stop the running command.
///
/// The command is spawned into a process group of its own, so the SIGINT the
/// terminal delivers to the foreground group reaches this process alone. Left at
/// its default, that would kill runandlog and leave the command running with
/// nothing written back -- exactly the output the user was waiting for. So the
/// signal is caught, the command is killed through `canceller`, and the partial
/// result still goes into the Markdown.
///
/// SIGTERM is treated the same way: it also means "stop", and the command would
/// otherwise be orphaned just as thoroughly.
#[cfg(unix)]
fn catch_interrupts(canceller: &Canceller) {
    extern "C" fn on_signal(signal: libc::c_int) {
        // `Canceller::cancel` cannot be called from here at all: it takes a lock, and
        // a signal that interrupts the polling thread while that thread holds the
        // lock would leave the handler waiting on itself. Raising a flag for the
        // thread below is the only thing a handler can safely leave behind.
        if INTERRUPTED_BY.swap(signal, Ordering::SeqCst) != 0 {
            // Already interrupted once and still here: the user is asking again, so
            // stop without waiting for the write-back.
            // SAFETY: _exit is async-signal-safe, unlike a normal exit.
            unsafe { libc::_exit(signal_exit_code(signal) as libc::c_int) };
        }
    }

    // SAFETY: installing a handler that only touches an atomic and _exit.
    unsafe {
        let handler = on_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    let canceller = canceller.clone();
    std::thread::spawn(move || {
        while INTERRUPTED_BY.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        canceller.cancel();
    });
}

/// The signal that interrupted the run, or zero while none has arrived.
///
/// Kept rather than a plain flag so the exit code can say *which* signal stopped
/// the run: a job runner that sent SIGTERM should not be told the run was
/// interrupted at a keyboard.
#[cfg(unix)]
static INTERRUPTED_BY: AtomicI32 = AtomicI32::new(0);

/// Whether a signal asking the run to stop has arrived.
///
/// Read straight from what the handler sets, so that it is true the moment the
/// signal lands rather than once the watching thread has noticed.
#[cfg(unix)]
fn interrupt_requested() -> bool {
    INTERRUPTED_BY.load(Ordering::SeqCst) != 0
}

/// Exit code for the interrupted run.
#[cfg(unix)]
fn interrupt_exit_code() -> u8 {
    signal_exit_code(INTERRUPTED_BY.load(Ordering::SeqCst))
}

/// Nothing interrupts a run off unix, so these are only here to keep the caller
/// compiling.
#[cfg(not(unix))]
fn interrupt_requested() -> bool {
    false
}

#[cfg(not(unix))]
fn interrupt_exit_code() -> u8 {
    signal_exit_code(2)
}

/// Off unix the signal handling above has no equivalent, and `exec` cannot kill a
/// process group there either. Ctrl-C keeps its default meaning.
#[cfg(not(unix))]
fn catch_interrupts(_canceller: &Canceller) {}

/// Opens the desktop app.
///
/// Built only when the `gui` feature is on. Without it the binary still accepts
/// `--gui` so that the message explains what happened, rather than clap reporting
/// an unknown flag.
#[cfg(feature = "gui")]
fn open_gui(session: Session) -> std::io::Result<()> {
    runandlog::gui::run(session)
}

#[cfg(not(feature = "gui"))]
fn open_gui(_session: Session) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "this build has no GUI; rebuild with the \"gui\" feature to use --gui",
    ))
}

fn exec_options(args: &Args) -> ExecOptions {
    let cwd = args.cwd.clone().unwrap_or_else(|| {
        // Resolve first: Session::load canonicalizes the document, so with a symlink
        // argument the directory "containing the Markdown file" is the one holding
        // the real file, not the one holding the link.
        let file = args
            .file
            .canonicalize()
            .unwrap_or_else(|_| args.file.clone());
        file.parent()
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

/// Decides which cells to run, as 0-based indices.
///
/// Only called once the flags have established that this is a non-interactive
/// run, so an empty result means "run nothing", not "open the TUI".
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
    fn the_exit_code_names_the_signal_that_stopped_the_run() {
        // A job runner that sent SIGTERM must not be told the run was interrupted at
        // a keyboard, so the two cannot share one code.
        assert_eq!(signal_exit_code(2), 130);
        assert_eq!(signal_exit_code(15), 143);
    }

    #[test]
    fn shows_ellipsis_for_multi_line_commands() {
        assert_eq!(first_line("ls /opt\nls /tmp\n"), "ls /opt ...");
        assert_eq!(first_line("date\n"), "date");
        assert_eq!(first_line("\n\n"), "");
    }
}
