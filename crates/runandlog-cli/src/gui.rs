//! Desktop app (GUI).
//!
//! The window shows the same document the TUI shows: the file path, then every
//! cell with its command, a Run button, and the result of the last run. Parsing,
//! running and writing back all go through [`Session`], which is the same path
//! the TUI and non-interactive runs take. Nothing about the Markdown format is
//! re-implemented here, so the rules cannot drift apart.
//!
//! Design notes:
//!
//! - **A run happens on a blocking worker, never on a Tauri command thread.**
//!   `runandlog_core::run` blocks until the command finishes; running it inline
//!   would freeze the window for as long as the command takes.
//! - **The session is only locked around the short synchronous parts.** The lock
//!   is never held across an `await`, so a long run does not block the commands
//!   that read the document.
//! - **Only one run is in flight at a time** (`busy`). Cells share one file, and
//!   two concurrent write-backs would each see the other's edit as an external
//!   modification and refuse to write.
//! - **The whole document is re-sent after every write.** Writing back re-parses
//!   the file, so byte offsets and result blocks change; sending a diff would
//!   mean tracking that in two places.

use std::io;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use runandlog_core::Canceller;

use crate::session::Session;

/// Event carrying the current document. Sent whenever the file is re-read.
const EVENT_DOCUMENT: &str = "runandlog://document";
/// Event marking the start of a run.
const EVENT_STARTED: &str = "runandlog://started";
/// Event carrying a piece of a running command's output.
const EVENT_OUTPUT: &str = "runandlog://output";
/// Event marking the end of a run, successful or not.
const EVENT_FINISHED: &str = "runandlog://finished";

/// How much of one piece of output is sent to the window at a time.
///
/// A command can print faster than anything can be read, and the window only shows
/// the tail of what it has been sent. Sending megabytes through the IPC for a view
/// that drops all but the end of it is work with nothing to show for it, so an
/// overlong piece is cut to its end. Nothing is lost by it: the whole output is
/// written back to the Markdown when the command finishes.
const MAX_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

/// A cell as the window shows it.
#[derive(Debug, Clone, Serialize)]
struct CellView {
    /// Zero-based index, used to ask for a run.
    index: usize,
    /// One-based number, as shown.
    number: usize,
    lang: String,
    command: String,
    /// Destination file for the result, when the cell designates one.
    out_file: Option<String>,
    /// Body of the result block from the last run, without the markers.
    result: Option<String>,
}

/// The document as the window shows it.
#[derive(Debug, Clone, Serialize)]
struct DocumentView {
    path: String,
    cells: Vec<CellView>,
}

/// A piece of what a running command has printed.
///
/// Carries the cell it belongs to: a piece that arrives after the window has moved
/// on -- the last of a run whose result is already drawn, say -- must not be shown
/// under whatever cell is running now.
#[derive(Debug, Clone, Serialize)]
struct OutputChunk {
    index: usize,
    text: String,
}

/// How a run ended, for the status line.
#[derive(Debug, Clone, Serialize)]
struct RunReport {
    index: usize,
    /// `exit 0`, `timeout`, and so on.
    status: String,
    success: bool,
    /// Whether the run was stopped from the window. A batch stops here.
    cancelled: bool,
}

/// How a "run all" ended.
///
/// `stopped` is carried separately because a batch can end on a Stop that landed
/// between two cells, where there is no cancelled run to read it from.
#[derive(Debug, Clone, Serialize)]
struct BatchReport {
    reports: Vec<RunReport>,
    stopped: bool,
}

/// The operation in flight, if any, and what has been asked of it.
///
/// All three live under one lock rather than as separate atomics: they describe a
/// single thing, and a Stop has to be decided against the operation it was pressed
/// during. Read apart, "is anything running", "remember the stop" and "cancel the
/// command" can interleave with an operation ending and the next one starting, and
/// the stop then lands on a command the user never asked to stop.
#[derive(Default)]
struct Operation {
    /// Whether a run or a reload is in flight.
    busy: bool,
    /// Whether Stop was pressed during it.
    ///
    /// A canceller exists only while a command is actually running, and a batch
    /// spends real time between cells writing the previous result back. Without this
    /// a Stop landing in that gap would reach nothing and be forgotten, leaving the
    /// batch running after the window said it had stopped.
    stop_requested: bool,
    /// Handle for the command running right now.
    canceller: Option<Canceller>,
}

/// Shared state behind the Tauri commands.
struct GuiState {
    session: Mutex<Session>,
    operation: Mutex<Operation>,
}

impl GuiState {
    fn operation(&self) -> std::sync::MutexGuard<'_, Operation> {
        // Nothing under this lock can panic while it is held, so poisoning would
        // only be a leftover from an unrelated crash.
        self.operation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Publishes the handle for the command about to run, so Stop can reach it.
    ///
    /// Reports whether a stop was already asked for, which the caller must honour:
    /// the window keeps Stop available for the whole operation, including the moment
    /// before the first command has a handle to cancel.
    fn arm(&self, canceller: Option<Canceller>) -> bool {
        let mut operation = self.operation();
        operation.canceller = canceller;
        operation.stop_requested
    }

    /// Stops the operation in flight. Reports whether there was one.
    ///
    /// The request is remembered for as long as the operation lasts, so that a batch
    /// stops even when the press lands between two cells.
    fn stop(&self) -> bool {
        let mut operation = self.operation();
        if !operation.busy {
            return false;
        }
        operation.stop_requested = true;
        if let Some(canceller) = &operation.canceller {
            canceller.cancel();
        }
        true
    }

    /// Whether Stop has been pressed during the operation in flight.
    fn stop_requested(&self) -> bool {
        self.operation().stop_requested
    }

    /// Marks an operation as started, or reports that one is already in flight.
    ///
    /// The guard clears the mark on drop, so an early return or a panic in a command
    /// cannot leave the app permanently refusing to run anything.
    fn acquire(&self) -> Result<BusyGuard<'_>, String> {
        let mut operation = self.operation();
        if operation.busy {
            return Err("A command is already running.".to_string());
        }
        // A stop belongs to the operation it was pressed during, so this one starts
        // with a clean slate.
        *operation = Operation {
            busy: true,
            ..Operation::default()
        };
        Ok(BusyGuard { state: self })
    }
}

/// Ends the operation when it goes out of scope.
struct BusyGuard<'a> {
    state: &'a GuiState,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        let mut operation = self.state.operation();
        operation.busy = false;
        operation.canceller = None;
    }
}

/// Builds the view of the document currently held in memory.
fn document_view(session: &Session) -> DocumentView {
    let doc = session.doc();
    let cells = doc
        .cells
        .iter()
        .map(|cell| CellView {
            index: cell.index,
            number: cell.display_number(),
            lang: cell.lang.clone(),
            command: cell.command.clone(),
            out_file: cell.out_file.clone(),
            result: doc.result_text(cell).map(str::to_string),
        })
        .collect();
    DocumentView {
        path: session.path().display().to_string(),
        cells,
    }
}

/// Reads the document without touching the disk.
#[tauri::command]
fn document(state: State<'_, GuiState>) -> Result<DocumentView, String> {
    let session = state.session.lock().map_err(lock_error)?;
    Ok(document_view(&session))
}

/// Re-reads the file, picking up edits made in an external editor.
#[tauri::command]
fn reload(state: State<'_, GuiState>) -> Result<DocumentView, String> {
    reload_session(&state)
}

/// The body of [`reload`], separated from the Tauri wrapper so it can be tested
/// without an app handle.
///
/// **Reloading takes the busy flag too, so it cannot happen during a run.**
/// `Session::apply_outcome` refuses to write when the file changed while the
/// command was running, and it detects that by comparing the file against the
/// document it held *before* the run. A reload replaces that document with
/// whatever is on disk now, which makes the comparison pass again -- the check
/// would be defeated by the very thing it guards against, and the result of the
/// old command could land on a cell that is no longer the one it came from.
fn reload_session(state: &GuiState) -> Result<DocumentView, String> {
    let _busy = state
        .acquire()
        .map_err(|_| "A command is running, so the file cannot be reloaded yet.".to_string())?;
    let mut session = state.session.lock().map_err(lock_error)?;
    session.reload().map_err(|error| error.to_string())?;
    Ok(document_view(&session))
}

/// Runs one cell and writes the result back.
#[tauri::command]
async fn run_cell(
    app: AppHandle,
    state: State<'_, GuiState>,
    index: usize,
) -> Result<RunReport, String> {
    let _busy = state.acquire()?;
    execute(&app, &state, index).await
}

/// Runs every cell in order.
///
/// A failed command does not stop the batch -- its exit code is the result and
/// gets written back like any other. A failed *write-back* does stop it: the file
/// changed underneath us, so the commands held in memory may no longer be the
/// ones in the file.
#[tauri::command]
async fn run_all(app: AppHandle, state: State<'_, GuiState>) -> Result<BatchReport, String> {
    let _busy = state.acquire()?;
    let count = {
        let session = state.session.lock().map_err(lock_error)?;
        session.len()
    };

    let mut reports = Vec::new();
    let mut stopped = false;
    for index in 0..count {
        // Asked before the cell starts as well as after it ends. Writing the previous
        // result back takes real time, and a Stop landing in that gap has no command
        // to cancel -- starting this cell only to kill it at once would replace the
        // result it already had with an empty one.
        if state.stop_requested() {
            stopped = true;
            break;
        }
        match execute(&app, &state, index).await {
            Ok(report) => {
                stopped = report.cancelled;
                reports.push(report);
                if stopped {
                    // Stop was pressed. Its result has been written back; carrying on
                    // to the next cell is not what was asked for.
                    break;
                }
            }
            Err(error) => return Err(error),
        }
    }
    // Asked once more rather than left to the loop: a Stop landing while the last
    // cell's result is written back never reaches a check above, and the window
    // would report the batch as having run to the end after saying it was stopping.
    Ok(BatchReport {
        reports,
        stopped: stopped || state.stop_requested(),
    })
}

/// Stops the command in flight, keeping the output it has produced so far.
///
/// Reports whether there was anything to stop, so the window can tell "stopped"
/// from "nothing was running" without guessing.
#[tauri::command]
fn cancel(state: State<'_, GuiState>) -> bool {
    state.stop()
}

/// The body shared by [`run_cell`] and [`run_all`].
///
/// Split out so that the busy flag is taken once for a whole batch: taking it per
/// cell would let a second batch interleave with this one.
async fn execute(
    app: &AppHandle,
    state: &State<'_, GuiState>,
    index: usize,
) -> Result<RunReport, String> {
    let (command, options) = {
        let session = state.session.lock().map_err(lock_error)?;
        if index >= session.len() {
            return Err(format!("There is no cell {}.", index + 1));
        }
        (session.command_of(index), session.exec_options())
    };

    // One handle per run: a stopped cell must not leave the next one unable to start.
    let canceller = Canceller::new();
    if state.arm(Some(canceller.clone())) {
        // Stop was pressed before this command had a handle. Honour it here rather
        // than let the press vanish.
        canceller.cancel();
    }

    let _ = app.emit(EVENT_STARTED, index);
    // The lock is deliberately not held here: the run can take minutes, and the
    // window keeps reading the document while it does.
    let reporter = app.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        runandlog_core::run_streaming(&command, &options, &canceller, |chunk| {
            // Sent as the command prints rather than kept until it ends: a run that
            // takes minutes should show what it is doing while it does it.
            let _ = reporter.emit(
                EVENT_OUTPUT,
                OutputChunk {
                    index,
                    text: crate::live::tail(chunk, MAX_OUTPUT_CHUNK_BYTES).to_string(),
                },
            );
        })
    })
    .await
    .map_err(|error| format!("The worker thread died unexpectedly: {error}"))
    .inspect_err(|_| {
        state.arm(None);
    })?
    .map_err(|error| format!("The run failed: {error}"))
    .inspect_err(|_| {
        state.arm(None);
    })?;
    // Disarmed as soon as the command is over: from here on there is nothing to
    // stop, and a Stop that arrived late must not reach the *next* run.
    state.arm(None);

    let view = {
        let mut session = state.session.lock().map_err(lock_error)?;
        session
            .apply_outcome(index, &outcome)
            .map_err(|error| format!("Writing the result failed: {error}"))?;
        document_view(&session)
    };
    let _ = app.emit(EVENT_DOCUMENT, &view);
    let report = RunReport {
        index,
        status: outcome.status_text(),
        success: outcome.is_success(),
        cancelled: outcome.cancelled,
    };
    let _ = app.emit(EVENT_FINISHED, &report);
    Ok(report)
}

/// A poisoned lock means another thread panicked while holding the session.
fn lock_error<T>(_: std::sync::PoisonError<T>) -> String {
    "The session is no longer usable because a background task panicked.".to_string()
}

/// Opens the desktop app.
///
/// Mirrors [`crate::tui::run`]: it takes an already loaded session and returns
/// when the window closes.
pub fn run(session: Session) -> io::Result<()> {
    if let Some(reason) = no_display_reason() {
        return Err(io::Error::other(reason));
    }

    // The title is built before the session is handed over, so that the setup hook
    // does not have to take the lock just to read the file name.
    let title = window_title(session.path());

    tauri::Builder::default()
        .manage(GuiState {
            session: Mutex::new(session),
            operation: Mutex::new(Operation::default()),
        })
        .setup(move |app| {
            // Say which file is open. tauri.conf.json cannot express this because
            // the path is only known at run time.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title(&title);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            document, reload, run_cell, run_all, cancel
        ])
        .run(tauri::generate_context!())
        .map_err(io::Error::other)
}

/// Why the window cannot be opened, when there is no display to open it on.
///
/// GTK aborts the process with a panic when it cannot reach a display, which is
/// what happens over a plain SSH session or in a container. Running a command
/// line tool should not look like a crash, so the common case is caught here and
/// reported as an ordinary error, pointing at the TUI that does work there.
///
/// Only Linux needs this: macOS and Windows have no equivalent environment
/// variable, and their windowing systems are always available to a logged-in
/// user.
#[cfg(target_os = "linux")]
fn no_display_reason() -> Option<&'static str> {
    display_reason(
        std::env::var_os("DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
    )
}

/// The decision behind [`no_display_reason`], with the environment passed in.
///
/// Kept separate so the tests do not have to set environment variables: they are
/// process-wide, and the test harness runs tests on threads, so mutating them is
/// both unsound and able to disturb unrelated tests.
#[cfg(target_os = "linux")]
fn display_reason(
    display: Option<&std::ffi::OsStr>,
    wayland: Option<&std::ffi::OsStr>,
) -> Option<&'static str> {
    // An empty value is what a stripped SSH environment leaves behind, and it is
    // no more usable than an unset one.
    let usable = |value: Option<&std::ffi::OsStr>| value.is_some_and(|value| !value.is_empty());
    if usable(display) || usable(wayland) {
        return None;
    }
    Some(
        "no display is available (DISPLAY and WAYLAND_DISPLAY are both unset), so the GUI cannot open; drop --gui to use the TUI",
    )
}

#[cfg(not(target_os = "linux"))]
fn no_display_reason() -> Option<&'static str> {
    None
}

/// Window title: the file name, falling back to the whole path when there is no
/// file name to show (a path ending in `..`, for instance).
fn window_title(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    format!("Run and Log - {name}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use runandlog_core::ExecOptions;

    use super::*;

    /// Hands out a distinct number per directory.
    ///
    /// Tests run on threads of one process, so the pid alone does not separate
    /// them: two tests picking the same directory would delete it from under each
    /// other on the way in.
    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    /// A temporary directory for tests, including its own cleanup.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let path = std::env::temp_dir().join(format!(
                "runandlog-gui-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn session(path: &Path) -> Session {
        let mut options = ExecOptions::new(path.parent().unwrap());
        options.shell = PathBuf::from("/bin/sh");
        Session::load(path, options, 50).unwrap()
    }

    /// The caller keeps the directory alive: dropping it deletes the file the
    /// session was loaded from.
    fn state(dir: &TempDir) -> GuiState {
        GuiState {
            // The session is irrelevant to the busy flag, so any file will do.
            session: Mutex::new(session(&dir.write("doc.md", "# no cells\n"))),
            operation: Mutex::new(Operation::default()),
        }
    }

    #[test]
    fn the_title_shows_the_file_name() {
        assert_eq!(
            window_title(Path::new("/tmp/notes/exam.md")),
            "Run and Log - exam.md"
        );
    }

    #[test]
    fn the_title_falls_back_to_the_whole_path() {
        // A path with no file name still has to produce a title rather than panic.
        assert_eq!(window_title(Path::new("/tmp/..")), "Run and Log - /tmp/..");
    }

    #[test]
    fn a_second_run_is_refused_while_one_is_in_flight() {
        let dir = TempDir::new();
        let state = state(&dir);
        let _first = state.acquire().unwrap();
        // Two runs would write back to the same file and each would see the other's
        // edit as an external modification.
        assert!(state.acquire().is_err());
    }

    #[test]
    fn the_busy_flag_is_released_when_the_guard_is_dropped() {
        let dir = TempDir::new();
        let state = state(&dir);
        drop(state.acquire().unwrap());
        // Without the Drop impl an early return would leave the app refusing to run
        // anything for the rest of the session.
        assert!(state.acquire().is_ok());
    }

    #[test]
    fn stopping_while_idle_says_there_was_nothing_to_stop() {
        let dir = TempDir::new();
        let state = state(&dir);
        // The window uses this to tell "stopped" from "there was nothing to stop"
        // rather than reporting a stop that never happened.
        assert!(!state.stop());
        assert!(!state.stop_requested());
    }

    #[test]
    fn stopping_reaches_the_run_in_flight() {
        let dir = TempDir::new();
        let state = state(&dir);
        let _busy = state.acquire().unwrap();
        let canceller = Canceller::new();
        state.arm(Some(canceller.clone()));

        assert!(state.stop());
        assert!(canceller.is_cancelled());
    }

    #[test]
    fn a_stop_between_two_cells_still_stops_the_batch() {
        let dir = TempDir::new();
        let state = state(&dir);
        let _busy = state.acquire().unwrap();
        // Where `run_all` is between cells: the previous run is disarmed and the
        // next one has not started. There is no handle to cancel, so the press has
        // to be remembered instead of dropped.
        state.arm(None);

        assert!(state.stop());
        assert!(state.stop_requested());
    }

    #[test]
    fn a_stop_that_beats_the_command_to_the_start_is_honoured() {
        let dir = TempDir::new();
        let state = state(&dir);
        let _busy = state.acquire().unwrap();
        // The window enables Stop as soon as the operation begins, which is before
        // the first command exists.
        state.stop();

        let canceller = Canceller::new();
        // `execute` cancels when arming reports a pending stop.
        assert!(state.arm(Some(canceller.clone())));
    }

    #[test]
    fn a_stop_does_not_carry_over_to_the_next_run() {
        let dir = TempDir::new();
        let state = state(&dir);
        let busy = state.acquire().unwrap();
        let stopped = Canceller::new();
        state.arm(Some(stopped.clone()));
        state.stop();
        // Disarmed when the run ends, exactly as `execute` does it.
        state.arm(None);
        drop(busy);

        let _busy = state.acquire().unwrap();
        let next = Canceller::new();
        // Without a fresh handle per run, and without clearing the request when the
        // next operation starts, the next command would refuse to start.
        assert!(!state.arm(Some(next.clone())));
        assert!(!next.is_cancelled());
        assert!(stopped.is_cancelled());
    }

    #[test]
    fn reloading_is_refused_while_a_command_is_running() {
        let dir = TempDir::new();
        let state = state(&dir);
        let _running = state.acquire().unwrap();

        // Session::apply_outcome spots an external edit by comparing the file with
        // the document it held before the run. Reloading mid-run replaces that
        // document with the edited file, so the comparison passes and the result of
        // the old command can be written to whatever cell now sits at that index.
        assert!(reload_session(&state).is_err());
    }

    #[test]
    fn reloading_works_again_once_the_run_is_over() {
        let dir = TempDir::new();
        let state = state(&dir);
        drop(state.acquire().unwrap());

        // The guard above must not leave reloading blocked for the rest of the
        // session.
        assert!(reload_session(&state).is_ok());
    }

    #[test]
    fn reloading_picks_up_an_external_edit() {
        let dir = TempDir::new();
        let state = state(&dir);
        assert!(reload_session(&state).unwrap().cells.is_empty());

        dir.write("doc.md", "```shell\ndate\n```\n");
        let view = reload_session(&state).unwrap();

        assert_eq!(view.cells.len(), 1);
        assert_eq!(view.cells[0].command, "date\n");
    }

    #[test]
    fn the_view_carries_what_the_window_draws() {
        let dir = TempDir::new();
        let path = dir.write(
            "doc.md",
            "# notes\n\n```shell\ndate\n```\n\n```shell out=log.txt\nls\n```\n",
        );
        let view = document_view(&session(&path));

        assert_eq!(view.cells.len(), 2);
        assert_eq!(view.cells[0].index, 0);
        // The window labels cells the way the CLI and the TUI do, from 1.
        assert_eq!(view.cells[0].number, 1);
        assert_eq!(view.cells[0].command, "date\n");
        assert_eq!(view.cells[0].out_file, None);
        assert_eq!(view.cells[0].result, None);
        assert_eq!(view.cells[1].number, 2);
        assert_eq!(view.cells[1].out_file.as_deref(), Some("log.txt"));
    }

    #[test]
    fn the_view_carries_the_previous_result() {
        let dir = TempDir::new();
        let path = dir.write(
            "doc.md",
            "```shell\ndate\n```\n\n<!-- runandlog:begin -->\nRan result: earlier\n<!-- runandlog:end -->\n",
        );
        let view = document_view(&session(&path));

        // The window shows the last result on open, without re-running anything.
        assert_eq!(view.cells[0].result.as_deref(), Some("Ran result: earlier"));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod display_tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn a_missing_display_is_reported_instead_of_crashing_gtk() {
        // Without this check GTK aborts the process, which reads as a crash rather
        // than as "this machine has no screen".
        assert!(display_reason(None, None).is_some());
    }

    #[test]
    fn either_display_variable_is_enough() {
        assert!(display_reason(Some(OsStr::new(":0")), None).is_none());
        assert!(display_reason(None, Some(OsStr::new("wayland-0"))).is_none());
    }

    #[test]
    fn an_empty_value_does_not_count_as_a_display() {
        // A stripped SSH environment leaves the variable set but empty, and GTK
        // cannot connect to that any more than to an unset one.
        assert!(display_reason(Some(OsStr::new("")), Some(OsStr::new(""))).is_some());
    }
}
