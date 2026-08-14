//! Command execution.
//!
//! stdout and stderr are received through a single pipe. The goal is to write
//! them back to Markdown as one log with the original interleaving preserved,
//! so reading the two streams separately and concatenating them is not an option.

use std::io::{Read, pipe};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};

/// How often the child process is polled for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Grace period to wait for the reader thread after the process exits.
///
/// When a background grandchild process still holds the pipe -- which can happen
/// with no timeout set -- EOF never arrives, so the read is cut off here and
/// whatever was read becomes the result.
const DRAIN_GRACE: Duration = Duration::from_millis(300);
/// How long the reader may wait for data before re-checking its stop flag.
const READ_TIMEOUT: Duration = Duration::from_millis(100);
/// Default cap on captured output. A safeguard against a command that keeps
/// printing forever exhausting memory.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Execution settings.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Shell the command is handed to.
    pub shell: PathBuf,
    /// Working directory.
    pub cwd: PathBuf,
    /// Deadline for the run. `None` means no limit.
    pub timeout: Option<Duration>,
    /// Cap on captured output in bytes. Anything beyond it is discarded and
    /// `truncated` is set.
    pub max_output_bytes: usize,
}

impl ExecOptions {
    /// Takes `cwd` and uses the `SHELL` environment variable (falling back to
    /// `/bin/sh`) as the shell.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        ExecOptions {
            shell,
            cwd: cwd.into(),
            timeout: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// The outcome of a single run.
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    /// When the run started (local time).
    pub started_at: DateTime<Local>,
    /// How long the run took.
    pub duration: Duration,
    /// Exit code. `None` when the process was terminated by a signal.
    pub exit_code: Option<i32>,
    /// stdout and stderr combined.
    pub output: String,
    /// Whether the run was cut off by the timeout.
    pub timed_out: bool,
    /// Whether the output was truncated at the cap.
    pub truncated: bool,
}

impl ExecOutcome {
    /// Whether the run counts as successful.
    pub fn is_success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    /// A short description of how the run ended.
    pub fn status_text(&self) -> String {
        if self.timed_out {
            return "timeout".to_string();
        }
        match self.exit_code {
            Some(code) => format!("exit {code}"),
            None => "signaled".to_string(),
        }
    }
}

/// Runs `command` through a shell.
///
/// `Err` is returned only when the shell itself fails to start. A command that
/// merely fails comes back as an `ExecOutcome` carrying its exit code.
pub fn run(command: &str, options: &ExecOptions) -> std::io::Result<ExecOutcome> {
    // Spawning in a directory that does not exist surfaces only as "the shell
    // failed to start", which says nothing about the real cause.
    if !options.cwd.is_dir() {
        return Err(std::io::Error::other(format!(
            "working directory does not exist: {}",
            options.cwd.display()
        )));
    }

    let started_at = Local::now();
    let start = Instant::now();

    let (mut reader, writer) = pipe()?;
    let writer_for_stderr = writer.try_clone()?;
    let mut builder = Command::new(&options.shell);
    builder
        .arg("-c")
        .arg(command)
        .current_dir(&options.cwd)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(writer_for_stderr);
    // Put the shell in its own process group so a timeout can take down its
    // descendants along with it.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut builder, 0);
    let child = builder.spawn()?;
    // Drop the builder so the parent releases its copies of the pipe's write end.
    // The child inherits its own; while the parent still holds one, the reader
    // thread never reaches EOF and every run pays the full DRAIN_GRACE.
    drop(builder);

    // The reader thread reads until EOF. It will not finish while a grandchild
    // process holds the pipe, so the buffer is shared behind a lock and the main
    // thread moves on without joining it.
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let truncated = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (drained_tx, drained_rx) = mpsc::channel();
    let reader_buffer = Arc::clone(&buffer);
    let reader_truncated = Arc::clone(&truncated);
    let reader_stop = Arc::clone(&stop);
    let limit = options.max_output_bytes;
    // Without this the `stop` flag below is toothless: a silent grandchild
    // (`sleep 86400 &`) holds the pipe open and produces nothing, so the reader
    // stays parked inside `read` forever and never looks at the flag again. The
    // thread, its pipe descriptor and the captured buffer would then live until
    // that descendant exits, and repeated runs would pile them up.
    make_reads_interruptible(&reader);
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            if reader_stop.load(Ordering::Relaxed) {
                break;
            }
            if !wait_readable(&reader) {
                // Nothing arrived in time. Loop back so the stop flag gets another look.
                continue;
            }
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if is_retryable(&error) => continue,
                Err(_) => break,
            };
            let mut buffer = reader_buffer.lock().unwrap();
            let room = limit.saturating_sub(buffer.len());
            if room == 0 {
                // Discard it. Stopping the read would fill the pipe and stall
                // the command.
                reader_truncated.store(true, Ordering::Relaxed);
                continue;
            }
            if read > room {
                reader_truncated.store(true, Ordering::Relaxed);
            }
            buffer.extend_from_slice(&chunk[..read.min(room)]);
        }
        let _ = drained_tx.send(());
    });

    let child = Arc::new(Mutex::new(child));
    let mut timed_out = false;
    let exit_code = loop {
        let status = child.lock().unwrap().try_wait()?;
        if let Some(status) = status {
            break status.code();
        }
        if let Some(limit) = options.timeout
            && start.elapsed() >= limit
        {
            timed_out = true;
            let mut child = child.lock().unwrap();
            kill_process_group(&mut child);
            break child.wait()?.code();
        }
        thread::sleep(POLL_INTERVAL);
    };
    // Take the elapsed time here: waiting for the reader to drain is bookkeeping,
    // not part of how long the command took.
    let duration = start.elapsed();

    if drained_rx.recv_timeout(DRAIN_GRACE).is_err() {
        // A grandchild is still holding the pipe and producing output. Tell the
        // reader thread to stop at its next read so it does not run forever.
        stop.store(true, Ordering::Relaxed);
    }
    let output = String::from_utf8_lossy(&buffer.lock().unwrap()).to_string();

    Ok(ExecOutcome {
        started_at,
        duration,
        exit_code,
        output,
        timed_out,
        truncated: truncated.load(Ordering::Relaxed),
    })
}

/// Puts the pipe in non-blocking mode so a read can never park indefinitely.
///
/// Paired with [`wait_readable`]: the wait is what avoids a busy loop, and
/// non-blocking mode is what guarantees the read itself always returns, even if the
/// readiness reported by `poll` turns out to be spurious.
///
/// A read *timeout* would be the tidier mechanism, but `SO_RCVTIMEO` is a socket
/// option and silently does nothing on a pipe.
#[cfg(unix)]
fn make_reads_interruptible(reader: &std::io::PipeReader) {
    use std::os::fd::AsRawFd;

    // SAFETY: `reader` owns the descriptor and outlives both calls.
    unsafe {
        let flags = libc::fcntl(reader.as_raw_fd(), libc::F_GETFL);
        if flags != -1 {
            libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(not(unix))]
fn make_reads_interruptible(_reader: &std::io::PipeReader) {}

/// Waits for the pipe to have data, giving up after [`READ_TIMEOUT`].
///
/// Returns false if nothing arrived in time, which is the reader's cue to look at
/// its stop flag again. Blocking here rather than spinning keeps an ordinary run
/// from burning CPU, and waking on readiness keeps it from adding latency.
#[cfg(unix)]
fn wait_readable(reader: &std::io::PipeReader) -> bool {
    use std::os::fd::AsRawFd;

    let mut poll_fd = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: a single valid pollfd is passed, and `reader` outlives the call.
    let ready = unsafe { libc::poll(&mut poll_fd, 1, READ_TIMEOUT.as_millis() as libc::c_int) };
    // On error, report readiness and let the non-blocking read settle it.
    ready != 0
}

#[cfg(not(unix))]
fn wait_readable(_reader: &std::io::PipeReader) -> bool {
    true
}

/// Whether a read failed only because no data was available yet.
fn is_retryable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

/// Terminates the shell together with all of its descendants.
///
/// Killing only the shell leaves the commands it started running, with the pipe
/// still open.
#[cfg(unix)]
fn kill_process_group(child: &mut Child) {
    // Spawned with process_group(0), so the process group id equals the child's pid.
    let pid = child.id() as i32;
    // SAFETY: this only signals a process group that we spawned ourselves.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ExecOptions {
        ExecOptions {
            shell: PathBuf::from("/bin/sh"),
            cwd: std::env::temp_dir(),
            timeout: Some(Duration::from_secs(10)),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    #[test]
    fn captures_stdout() {
        let outcome = run("echo hello", &options()).unwrap();
        assert_eq!(outcome.output, "hello\n");
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.is_success());
        assert!(!outcome.truncated);
    }

    #[test]
    fn captures_stderr_together_with_stdout() {
        let outcome = run("echo out; echo err 1>&2", &options()).unwrap();
        assert!(outcome.output.contains("out"));
        assert!(outcome.output.contains("err"));
    }

    #[test]
    fn reports_exit_code() {
        let outcome = run("exit 3", &options()).unwrap();
        assert_eq!(outcome.exit_code, Some(3));
        assert!(!outcome.is_success());
        assert_eq!(outcome.status_text(), "exit 3");
    }

    #[test]
    fn runs_multiple_lines_in_one_shell() {
        let outcome = run("a=1\necho \"value=$a\"", &options()).unwrap();
        assert_eq!(outcome.output, "value=1\n");
    }

    #[test]
    fn runs_in_the_given_directory() {
        let dir = std::env::temp_dir();
        let mut opts = options();
        opts.cwd = dir.clone();
        let outcome = run("pwd", &opts).unwrap();
        assert!(!outcome.output.trim().is_empty());
    }

    #[test]
    fn rejects_a_missing_working_directory() {
        let mut opts = options();
        opts.cwd = std::env::temp_dir().join("runandlog-no-such-dir");
        let error = run("pwd", &opts).unwrap_err();
        assert!(error.to_string().contains("working directory"));
    }

    #[test]
    fn a_fast_command_does_not_wait_for_the_drain_grace() {
        // The parent must release its copies of the pipe's write end after spawning.
        // If it holds one, the reader never sees EOF and every run costs DRAIN_GRACE.
        //
        // This has to measure the wall clock around `run`, not `ExecOutcome::duration`:
        // the latter is taken before the drain wait, so it stays small even when the
        // bug is present.
        //
        // The threshold cannot be raised above DRAIN_GRACE without going blind --
        // DRAIN_GRACE *is* the cost of the regression. What keeps this stable is the
        // margin below it: with the write end released, a run takes single-digit
        // milliseconds, so there are two orders of magnitude of headroom. The
        // regression costs DRAIN_GRACE on *every* run, so taking the fastest of
        // several attempts detects it just as well while letting a loaded machine
        // lose individual runs to scheduling.
        const ATTEMPTS: usize = 5;
        let fastest = (0..ATTEMPTS)
            .map(|_| {
                let start = Instant::now();
                run("echo quick", &options()).unwrap();
                start.elapsed()
            })
            .min()
            .unwrap();
        assert!(
            fastest < DRAIN_GRACE,
            "the fastest of {ATTEMPTS} runs took {fastest:?}, at least the drain grace of {DRAIN_GRACE:?}"
        );
    }

    #[test]
    fn kills_the_command_on_timeout() {
        let mut opts = options();
        opts.timeout = Some(Duration::from_millis(200));
        let outcome = run("sleep 30", &opts).unwrap();
        assert!(outcome.timed_out);
        assert_eq!(outcome.status_text(), "timeout");
        assert!(outcome.duration < Duration::from_secs(5));
    }

    #[test]
    fn timeout_kills_grandchildren_that_hold_the_pipe() {
        let mut opts = options();
        opts.timeout = Some(Duration::from_millis(200));
        let start = Instant::now();
        // While a grandchild holds the pipe, killing just the shell never yields EOF.
        let outcome = run("sleep 30 & wait", &opts).unwrap();
        assert!(outcome.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_reader_thread_stops_when_a_silent_grandchild_holds_the_pipe() {
        // `sleep` keeps the pipe open and writes nothing, so the reader is parked in
        // `read` with no data ever coming. Unless reads are interruptible, the stop
        // flag never gets looked at again and the thread, its descriptor and the
        // captured buffer leak -- once per run.
        fn threads() -> usize {
            std::fs::read_dir("/proc/self/task").unwrap().count()
        }

        let mut opts = options();
        opts.timeout = None;
        let before = threads();
        for _ in 0..5 {
            let outcome = run("sleep 20 &", &opts).unwrap();
            assert!(outcome.is_success());
        }
        // The stop flag is set after DRAIN_GRACE and acted on within READ_TIMEOUT.
        thread::sleep(DRAIN_GRACE + READ_TIMEOUT * 4);
        let after = threads();
        assert!(
            after <= before + 1,
            "reader threads leaked: {before} -> {after}"
        );
    }

    #[test]
    fn truncates_output_beyond_the_limit() {
        let mut opts = options();
        opts.max_output_bytes = 1024;
        let outcome = run("seq 1 100000", &opts).unwrap();
        assert!(outcome.truncated);
        assert!(outcome.output.len() <= 1024);
    }
}
