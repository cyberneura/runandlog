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
use std::sync::{Arc, Mutex, PoisonError};
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

/// A handle for stopping a run from another thread.
///
/// The command is deliberately given a process group of its own, so the SIGINT a
/// terminal sends on Ctrl-C goes to the front end alone and never reaches the
/// command. Signalling it is therefore the front end's job, and this is the handle
/// it uses: hold a clone, and call [`Canceller::cancel`] when the user asks to stop.
///
/// `cancel` takes a lock, so it must not be called from a signal handler. A handler
/// can raise a flag for an ordinary thread to act on instead.
#[derive(Debug, Clone, Default)]
pub struct Canceller(Arc<CancelState>);

#[derive(Debug, Default)]
struct CancelState {
    requested: AtomicBool,
    /// Process group of the running command, which is the shell's pid. Zero while
    /// nothing is running.
    ///
    /// A lock rather than an atomic, because reading the id and killing it has to
    /// exclude the moment the command is reaped. A thread can be descheduled for an
    /// unbounded time between the two, and once the command has been reaped its pid
    /// is free to be handed to something else -- which the kill would then hit.
    group: Mutex<i32>,
}

impl Canceller {
    pub fn new() -> Canceller {
        Canceller::default()
    }

    /// Asks the running command to stop, killing its whole process group.
    ///
    /// A command that has not been spawned yet is covered too: `run` looks at the
    /// flag on every poll, so it kills the command as soon as it has one.
    pub fn cancel(&self) {
        // Written before the group is read, while `run` writes the group before it
        // reads the flag. One of the two therefore always sees the other's value,
        // so a cancel arriving in the middle of spawning cannot be lost.
        self.0.requested.store(true, Ordering::SeqCst);
        // Held across the kill on purpose: `run_cancellable` clears the id in the
        // same critical section that reaps the command, so an id read here always
        // names a process that has not been reaped -- and whose number therefore
        // cannot yet have been given to anything else.
        let group = self.group();
        kill_group(*group);
    }

    /// Whether a stop has been asked for.
    pub fn is_cancelled(&self) -> bool {
        self.0.requested.load(Ordering::SeqCst)
    }

    fn group(&self) -> std::sync::MutexGuard<'_, i32> {
        // The lock guards an i32 and is only ever held across `kill` / `try_wait`,
        // neither of which can panic, so poisoning is not something callers should
        // have to handle.
        self.0.group.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_group(&self, pid: i32) {
        *self.group() = pid;
    }

    /// Polls the command, forgetting its group as soon as it has ended.
    ///
    /// `poll` reaps the command when it reports one has ended, so the id has to stop
    /// being killable in the same breath -- under the lock a cancel needs.
    fn poll_with_group<T>(
        &self,
        poll: impl FnOnce() -> std::io::Result<Option<T>>,
    ) -> std::io::Result<Option<T>> {
        let mut group = self.group();
        let status = poll()?;
        if status.is_some() {
            *group = 0;
        }
        Ok(status)
    }

    /// Kills the command's process group and forgets it.
    ///
    /// The caller reaps the command afterwards, outside the lock, since waiting for
    /// it to die would block every cancel meanwhile.
    fn kill_and_forget(&self, child: &mut Child) {
        let mut group = self.group();
        kill_process_group(child);
        *group = 0;
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
    /// Whether the run was stopped through a [`Canceller`].
    pub cancelled: bool,
    /// Whether the output was truncated at the cap.
    pub truncated: bool,
}

impl ExecOutcome {
    /// Whether the run counts as successful.
    pub fn is_success(&self) -> bool {
        !self.timed_out && !self.cancelled && self.exit_code == Some(0)
    }

    /// A short description of how the run ended.
    pub fn status_text(&self) -> String {
        if self.timed_out {
            return "timeout".to_string();
        }
        if self.cancelled {
            return "cancelled".to_string();
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
///
/// Nothing can stop the run but its timeout. Use [`run_cancellable`] where the
/// user needs a way out of a command that hangs.
pub fn run(command: &str, options: &ExecOptions) -> std::io::Result<ExecOutcome> {
    run_cancellable(command, options, &Canceller::new())
}

/// Runs `command` through a shell, watching `canceller` for a request to stop.
///
/// On a stop the whole process group is killed and the outcome comes back with
/// `cancelled` set, carrying whatever the command had printed so far -- the point
/// of stopping is to keep that output, not to throw it away.
pub fn run_cancellable(
    command: &str,
    options: &ExecOptions,
    canceller: &Canceller,
) -> std::io::Result<ExecOutcome> {
    run_streaming(command, options, canceller, |_| {})
}

/// Runs `command` through a shell, handing its output to `on_output` as it
/// arrives.
///
/// This is [`run_cancellable`] with a window onto a run in progress, for front
/// ends that show what a command is printing rather than only what it printed.
/// The chunks concatenate into exactly [`ExecOutcome::output`], so a front end can
/// draw them as they come without having to reconcile them with the finished
/// result afterwards.
///
/// `on_output` runs on a thread of its own -- neither the one reading the pipe nor
/// the one supervising the command -- so taking a long time in it costs nothing but
/// the freshness of the display. It cannot stall the reader, which is what would
/// fill the pipe and stop the command itself, and it cannot hold up the timeout or
/// a cancellation.
///
/// It does hold up this call, though: the last piece of output is reported before
/// the outcome is returned, so a callback that never returns leaves the caller
/// waiting on itself. The command is not left running by it -- the timeout and any
/// cancellation have already been carried out by then.
pub fn run_streaming(
    command: &str,
    options: &ExecOptions,
    canceller: &Canceller,
    on_output: impl FnMut(&str) + Send,
) -> std::io::Result<ExecOutcome> {
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
    // Registered before the cancel flag is read below, so a cancel that lands
    // during the spawn is picked up by one side or the other.
    canceller.set_group(child.id() as i32);
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
    // Reporting the output as it arrives is left to a thread of its own, rather than
    // done between the polls below. Those polls are what enforce the timeout and
    // notice a cancellation, and a front end that blocks in its callback -- printing
    // to a pipe nobody is draining, say -- would otherwise let a command run for as
    // long as the front end took, whatever limit it was given.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    thread::scope(|scope| -> std::io::Result<ExecOutcome> {
        // The buffer is borrowed and the receiver moved: a `Receiver` may travel to
        // another thread but not be shared with one.
        let reported = &buffer;
        let reporter = scope.spawn(move || report_output(reported, done_rx, on_output));

        let mut timed_out = false;
        let mut cancelled = false;
        let exit_code = loop {
            // Asked before the child is polled, and not after: `cancel` kills the group
            // before this loop gets to look at anything, so polling first would find a
            // dead child and report a plain signal kill instead of a cancellation.
            if canceller.is_cancelled() {
                cancelled = true;
                let mut child = child.lock().unwrap();
                // `Canceller::cancel` has normally killed the group already; killing a
                // group that is gone is a no-op, and doing it here is what covers a
                // cancel that arrived before the command had a group to kill.
                canceller.kill_and_forget(&mut child);
                break child.wait()?.code();
            }
            let mut guard = child.lock().unwrap();
            let status = canceller.poll_with_group(|| guard.try_wait())?;
            drop(guard);
            if let Some(status) = status {
                break status.code();
            }
            if let Some(limit) = options.timeout
                && start.elapsed() >= limit
            {
                timed_out = true;
                let mut child = child.lock().unwrap();
                canceller.kill_and_forget(&mut child);
                break child.wait()?.code();
            }
            thread::sleep(POLL_INTERVAL);
        };
        // Take the elapsed time here: waiting for the reader to drain is bookkeeping,
        // not part of how long the command took.
        let duration = start.elapsed();
        // A cancel that lands between the flag being checked above and the command being
        // polled kills it without this loop ever taking the cancel branch: the poll finds
        // an already dead command and reports a plain signal kill. The flag is set before
        // the kill, so asking it here tells the two apart.
        let cancelled = cancelled || (canceller.is_cancelled() && exit_code.is_none());

        // Every path out of the loop above cleared the group in the same breath as the
        // reaping, so a cancel arriving from here on kills nothing. That includes this
        // drain wait: when a descendant outlives the shell and holds the pipe, it
        // survives a cancel that lands in the DRAIN_GRACE window. Keeping the id usable
        // that long is the worse trade -- the group may still hold those descendants, or
        // may be gone with its number already reissued, and from here the two cannot be
        // told apart.
        if drained_rx.recv_timeout(DRAIN_GRACE).is_err() {
            // A grandchild is still holding the pipe and producing output. Tell the
            // reader thread to stop at its next read so it does not run forever.
            stop.store(true, Ordering::Relaxed);
        }

        // Wakes the reporter now rather than at its next poll, so an ordinary run does
        // not end with a wait for a thread that has nothing left to do.
        drop(done_tx);
        // Joined before the last flush below: the reporter owns the callback until it
        // returns it, which is what keeps the callback from ever being called from two
        // threads at once.
        let (mut on_output, emitted) = match reporter.join() {
            Ok(reported) => reported,
            // A callback that panics used to unwind through this call. Keep it doing
            // so, rather than swallowing the panic on a thread the caller cannot see.
            Err(panic) => std::panic::resume_unwind(panic),
        };

        // Whatever arrived after the reporter's last flush -- including the bytes of a
        // character that was still incomplete then -- still has to be reported, or the
        // chunks would add up to all but the tail of the output. Both are taken from
        // one snapshot, under one lock, so nothing can slip in between them and leave
        // the two disagreeing.
        //
        // The callback is then called with the lock released. A run whose DRAIN_GRACE
        // expired leaves the reader thread still going, and holding the lock through a
        // slow callback would park it on the mutex -- which is the pipe going undrained,
        // the one thing a front end must never be able to cause.
        let (last, output) = {
            let captured = buffer.lock().unwrap();
            let last = (emitted < captured.len())
                .then(|| String::from_utf8_lossy(&captured[emitted..]).to_string());
            (last, String::from_utf8_lossy(&captured).to_string())
        };
        if let Some(last) = last {
            on_output(&last);
        }

        Ok(ExecOutcome {
            started_at,
            duration,
            exit_code,
            output,
            timed_out,
            cancelled,
            truncated: truncated.load(Ordering::Relaxed),
        })
    })
}

/// Hands the captured output to `on_output` until the run says it is over,
/// returning the callback along with how much of the output it has been given.
///
/// Runs on a thread of its own; see [`run_streaming`]. The last piece is left to the
/// caller, which can take it together with the finished output and so cannot cut the
/// two apart.
fn report_output<F: FnMut(&str)>(
    buffer: &Mutex<Vec<u8>>,
    done: mpsc::Receiver<()>,
    mut on_output: F,
) -> (F, usize) {
    let mut emitted = 0;
    loop {
        flush_output(buffer, &mut emitted, &mut on_output);
        match done.recv_timeout(POLL_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return (on_output, emitted),
            // Nothing yet: the command is still running, so keep reporting.
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Hands the callback the output captured since the last time, advancing
/// `emitted` past it.
///
/// The lock is released before the callback runs. The callback belongs to a front
/// end and may take real time -- drawing a screen, sending an event to a window --
/// and holding the lock through it would stall the reader thread, which fills the
/// pipe and stops the command itself.
fn flush_output(buffer: &Mutex<Vec<u8>>, emitted: &mut usize, on_output: &mut impl FnMut(&str)) {
    let chunk = {
        let buffer = buffer.lock().unwrap();
        let (chunk, used) = decode_ready(&buffer[*emitted..]);
        *emitted += used;
        chunk
    };
    if !chunk.is_empty() {
        on_output(&chunk);
    }
}

/// Decodes as much of `pending` as can be decoded without waiting for more bytes,
/// returning the text and how many bytes it consumed.
///
/// A character split across two reads is left behind rather than turned into a
/// replacement character: the rest of it is on its way, and reporting it whole one
/// poll later is better than reporting damage now. Bytes that cannot become a
/// character whatever follows them are replaced on the spot, the way
/// `String::from_utf8_lossy` would -- which is what makes the chunks add up to the
/// same text as the finished output.
fn decode_ready(pending: &[u8]) -> (String, usize) {
    let mut text = String::new();
    let mut used = 0;
    loop {
        let error = match std::str::from_utf8(&pending[used..]) {
            Ok(rest) => {
                text.push_str(rest);
                return (text, pending.len());
            }
            Err(error) => error,
        };
        if let Ok(head) = std::str::from_utf8(&pending[used..used + error.valid_up_to()]) {
            text.push_str(head);
        }
        used += error.valid_up_to();
        match error.error_len() {
            // The tail is the beginning of a character whose remaining bytes have
            // not been read yet.
            None => return (text, used),
            // Always at least one byte, so this cannot spin.
            Some(length) => {
                text.push(char::REPLACEMENT_CHARACTER);
                used += length;
            }
        }
    }
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

/// Not implemented off unix, where this module is already only half-working: see
/// the note on [`kill_process_group`]. A silent descendant therefore still parks the
/// reader, leaking the thread and its pipe handle for as long as that descendant
/// lives. Fixing it needs an overlapped/`WaitForMultipleObjects` read on Windows,
/// which cannot be written blind -- there is no way to build or test it here.
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
    kill_group(child.id() as i32);
}

/// Kills a process group by its id. A zero id means there is nothing running.
///
/// Callers reaching this through a [`Canceller`] hold its lock, and the reaping of a
/// command is done under that same lock together with clearing the id. A non-zero id
/// therefore always names a process that still exists -- which matters because once
/// a command has been reaped its pid is free to be given to anything else.
#[cfg(unix)]
fn kill_group(group: i32) {
    if group == 0 {
        return;
    }
    // SAFETY: the id is one this module spawned and has not reaped -- see above.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
}

/// Off unix there is no process group to kill, so this can only reach the shell
/// itself.
///
/// **Timeouts are therefore unreliable off unix**: the commands the shell started
/// keep running, and because they inherit the pipe, its write end stays open. That
/// is the same condition [`make_reads_interruptible`] cannot currently escape there.
/// Unix is the only supported target today; Windows needs job objects here and an
/// interruptible read there, and neither can be verified from this project's
/// development environment.
#[cfg(not(unix))]
fn kill_process_group(child: &mut Child) {
    let _ = child.kill();
}

/// Off unix a bare process group id is not something that can be signalled, so a
/// cancel only takes effect on the next poll inside [`run_cancellable`], where the
/// child handle is at hand.
#[cfg(not(unix))]
fn kill_group(_group: i32) {}

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

    // Counts threads through /proc, so it can only run on linux. The behaviour it
    // guards is not linux-specific; this is the only portable-enough way to observe it.
    #[cfg(target_os = "linux")]
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
    fn a_cancel_stops_a_running_command() {
        let mut opts = options();
        opts.timeout = None;
        let canceller = Canceller::new();
        let from_another_thread = canceller.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            from_another_thread.cancel();
        });
        let start = Instant::now();
        let outcome = run_cancellable("echo before; sleep 30", &opts, &canceller).unwrap();
        assert!(outcome.cancelled);
        assert!(!outcome.is_success());
        assert_eq!(outcome.status_text(), "cancelled");
        assert!(start.elapsed() < Duration::from_secs(5));
        // Stopping is meant to save the output, not discard it.
        assert!(outcome.output.contains("before"));
    }

    #[test]
    fn a_cancel_takes_the_descendants_that_hold_the_pipe_with_it() {
        // Killing only the shell leaves the sleep holding the pipe, and the run
        // would then sit in the drain wait with no EOF ever coming.
        let mut opts = options();
        opts.timeout = None;
        let canceller = Canceller::new();
        let from_another_thread = canceller.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            from_another_thread.cancel();
        });
        let start = Instant::now();
        let outcome = run_cancellable("sleep 30 & wait", &opts, &canceller).unwrap();
        assert!(outcome.cancelled);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_cancel_asked_for_before_the_run_still_stops_it() {
        // The command may not have a process group yet when the cancel arrives, so
        // the flag has to be honoured by the run itself rather than only by `kill`.
        let mut opts = options();
        opts.timeout = None;
        let canceller = Canceller::new();
        canceller.cancel();
        let start = Instant::now();
        let outcome = run_cancellable("sleep 30", &opts, &canceller).unwrap();
        assert!(outcome.cancelled);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_group_is_forgotten_together_with_the_reaping() {
        // Once reaped, the pid can be handed to any process. A cancel arriving after
        // a run has ended must therefore find nothing to kill -- however long the
        // thread that called it was descheduled on the way in. The clearing happens
        // inside the critical section that reaps, not before it.
        let canceller = Canceller::new();
        run_cancellable("echo done", &options(), &canceller).unwrap();
        assert_eq!(*canceller.group(), 0);

        let mut opts = options();
        opts.timeout = Some(Duration::from_millis(200));
        let canceller = Canceller::new();
        run_cancellable("sleep 30", &opts, &canceller).unwrap();
        assert_eq!(*canceller.group(), 0, "the timeout path forgot to clear it");
    }

    #[test]
    fn a_cancel_that_lands_between_the_check_and_the_poll_is_still_a_cancel() {
        // `cancel` kills the command itself, so the poll can find it already dead and
        // report a plain signal kill without this run ever taking the cancel branch.
        // Reporting "signaled" would then contradict what the caller was told.
        let mut opts = options();
        opts.timeout = None;
        let canceller = Canceller::new();
        let from_another_thread = canceller.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            from_another_thread.cancel();
        });
        let outcome = run_cancellable("sleep 30", &opts, &canceller).unwrap();
        assert!(outcome.cancelled);
        assert_eq!(outcome.status_text(), "cancelled");
    }

    #[test]
    fn an_unused_canceller_changes_nothing() {
        let canceller = Canceller::new();
        let outcome = run_cancellable("echo hello", &options(), &canceller).unwrap();
        assert!(!outcome.cancelled);
        assert!(outcome.is_success());
        assert!(!canceller.is_cancelled());
    }

    #[test]
    fn the_output_is_reported_while_the_command_is_still_running() {
        // The point of the callback: a front end should be able to show the first
        // line before the command that printed it has finished. Arriving in one
        // chunk at the end would satisfy "the chunks add up" while being no better
        // than reading `output`.
        let mut chunks = Vec::new();
        let outcome = run_streaming(
            "echo first; sleep 0.4; echo second",
            &options(),
            &Canceller::new(),
            |chunk| chunks.push(chunk.to_string()),
        )
        .unwrap();

        assert_eq!(outcome.output, "first\nsecond\n");
        assert!(
            chunks.first().is_some_and(|first| first == "first\n"),
            "expected the first line on its own, got {chunks:?}"
        );
        assert_eq!(chunks.concat(), outcome.output);
    }

    #[test]
    fn the_reported_chunks_add_up_to_the_whole_output() {
        // A front end draws the chunks and then the finished result. If the two
        // disagreed -- a dropped tail, a character reported twice -- the display
        // would change under the user when the command ended.
        let mut seen = String::new();
        let outcome = run_streaming(
            // Long enough to span several reads, and multi-byte so that a character
            // can land across the boundary between two of them.
            "for i in $(seq 1 400); do echo \"行 $i ✓\"; done",
            &options(),
            &Canceller::new(),
            |chunk| seen.push_str(chunk),
        )
        .unwrap();

        assert!(outcome.is_success());
        assert_eq!(seen, outcome.output);
    }

    #[test]
    fn a_cancelled_run_reports_what_it_printed() {
        let mut seen = String::new();
        let mut opts = options();
        opts.timeout = None;
        let canceller = Canceller::new();
        let from_another_thread = canceller.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            from_another_thread.cancel();
        });

        let outcome = run_streaming("echo before; sleep 30", &opts, &canceller, |chunk| {
            seen.push_str(chunk)
        })
        .unwrap();

        assert!(outcome.cancelled);
        assert_eq!(seen, outcome.output);
        assert!(seen.contains("before"));
    }

    #[test]
    fn a_front_end_that_blocks_does_not_hold_up_the_timeout() {
        // The polling that enforces the timeout must not be behind the callback. A
        // front end printing into a pipe nobody is draining blocks in there, and a
        // command given a limit would then run for as long as the front end took.
        let mut opts = options();
        opts.timeout = Some(Duration::from_millis(200));
        let start = Instant::now();

        let outcome = run_streaming("echo hello; sleep 30", &opts, &Canceller::new(), |_| {
            thread::sleep(Duration::from_secs(1));
        })
        .unwrap();

        assert!(outcome.timed_out);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the run took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_character_split_across_two_reads_is_held_back_until_it_is_whole() {
        // Reporting the first byte of a character on its own would put a replacement
        // character in the middle of the stream, where the finished output has none.
        let text = "行".as_bytes();
        let (early, used) = decode_ready(&text[..1]);
        assert_eq!(early, "");
        assert_eq!(used, 0);

        let (whole, used) = decode_ready(text);
        assert_eq!(whole, "行");
        assert_eq!(used, text.len());
    }

    #[test]
    fn bytes_that_cannot_become_a_character_are_replaced_rather_than_awaited() {
        // Waiting for the rest of a sequence that can never be valid would stall the
        // live display for the whole run.
        let (text, used) = decode_ready(b"a\xffb");
        assert_eq!(text, "a\u{fffd}b");
        assert_eq!(used, 3);
        // The same substitution `String::from_utf8_lossy` makes, so the chunks and
        // the finished output stay identical.
        assert_eq!(text, String::from_utf8_lossy(b"a\xffb"));
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
