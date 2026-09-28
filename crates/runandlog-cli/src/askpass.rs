//! Lets a command ask the user for a password.
//!
//! Commands run without a terminal (see `runandlog_core::run_streaming`), so a
//! program such as sudo cannot prompt on one. What those programs can do instead is
//! run an *askpass helper*: a program named in an environment variable, which they
//! call with the prompt as its argument and whose stdout they read as the answer.
//! sudo reads `SUDO_ASKPASS`, ssh `SSH_ASKPASS`, git `GIT_ASKPASS`.
//!
//! The helper is runandlog itself, reached through a symlink named
//! [`HELPER_NAME`]. Started that way, it forwards the prompt over a Unix socket to the
//! runandlog that ran the command, which asks the user through whatever front end
//! it has -- the terminal, the TUI, or the window -- and sends the answer back.
//!
//! The socket and the symlink live in a directory only the user can enter (0700),
//! created for this process and removed when the [`Askpass`] is dropped. The answer
//! is never written anywhere else: not to the captured output, not to the Markdown.
//!
//! Each run of a command is numbered, and its helper sends the number with the
//! prompt ([`RUN_VAR`]). Only the helpers of the run in progress are answered --
//! there is one at most, and none between runs. A process a previous cell left
//! behind in the background, still holding that cell's environment, cannot put a
//! question up while the next cell is running and have it taken for the next
//! cell's, nor between cells or after the last one. What the user types goes to
//! the command they were shown.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runandlog_core::ExecOptions;

use crate::session::RunHook;

/// File name the helper is started under. `main` checks for it before parsing
/// arguments, since the helper is called with a prompt, not with a Markdown file.
pub const HELPER_NAME: &str = "runandlog-askpass";
/// Variable through which the helper finds the socket.
const SOCKET_VAR: &str = "RUNANDLOG_ASKPASS_SOCKET";
/// Variable naming the run the command belongs to. Set afresh for every run; the
/// helper sends it ahead of the prompt and the listener answers only the current
/// run's helpers.
const RUN_VAR: &str = "RUNANDLOG_ASKPASS_RUN";
/// The variables the helper is offered under.
///
/// One the user has already set is left alone: an askpass they chose -- a graphical
/// one, say -- is a better answer than ours.
const HELPER_VARS: [&str; 3] = ["SUDO_ASKPASS", "SSH_ASKPASS", "GIT_ASKPASS"];
/// Longest prompt accepted from the helper. Prompts are a few lines of text at
/// most; anything longer is not one.
const MAX_PROMPT_BYTES: usize = 4096;
/// How long the helper may take to send its prompt once connected.
const PROMPT_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Ends the prompt on the wire. The helper keeps its side of the connection open
/// after it -- that is how the listener knows it is still there (see `answer`) --
/// so something other than end-of-file has to say where the prompt stops. Prompts
/// are text, which never contains a NUL.
const PROMPT_END: u8 = 0;
/// Reply prefixes. The answer follows `OK` on the same connection.
const REPLY_OK: &[u8] = b"OK\n";
const REPLY_CANCEL: &[u8] = b"CANCEL\n";
/// How often a prompter checks whether its answer is still wanted.
pub const PROMPT_POLL: Duration = Duration::from_millis(100);

/// Asks the user the prompt and returns what they typed, or `None` when they
/// declined -- or when there is no way to ask them at all.
///
/// Called on a thread of its own, one prompt at a time, while the command waits.
/// The second argument says whether the answer is still wanted: it turns true once
/// the helper that asked has gone (its command ended, timed out or was stopped) or
/// the [`Askpass`] is being dropped. **A prompter must look at it at least every
/// [`PROMPT_POLL`] and give up once it is true** -- dropping an `Askpass` waits for
/// the prompt in progress, which is what lets the terminal prompt put the terminal
/// back before the process exits.
pub type Prompter = Box<dyn Fn(&str, &dyn Fn() -> bool) -> Option<String> + Send + 'static>;

/// The listening end. Commands see it through the environment [`Askpass::apply`]
/// adds to their options.
pub struct Askpass {
    dir: PathBuf,
    shared: Arc<Shared>,
    listener: Option<thread::JoinHandle<()>>,
}

/// What the listener thread and the run-time side both look at.
struct Shared {
    socket: PathBuf,
    helper: PathBuf,
    closing: AtomicBool,
    /// The run whose helpers are answered. Zero while no run is in progress --
    /// before the first, between runs, after the last -- and then nothing is
    /// answered.
    ///
    /// A lock rather than an atomic because an answer is delivered *under* it:
    /// the run cannot end between the check that it is still on and the write,
    /// and once [`RunInProgress::drop`] has returned, no answer of that run is
    /// still on its way.
    current_run: Mutex<u64>,
    /// Number for the next run. Never reused, so a helper of an old run can never
    /// match a new one.
    next_run: AtomicU64,
}

/// A run in progress, as far as the helper is concerned. Its helpers are answered
/// until it is dropped, and refused from then on.
///
/// Dropped when the command has ended -- by the [`crate::session::Run`] that holds
/// it, before the write-back. A process the command left behind is then a process
/// of no run.
#[must_use = "dropping this ends the run: the helper refuses its prompts from then on"]
pub struct RunInProgress {
    shared: Arc<Shared>,
    run: u64,
}

impl Drop for RunInProgress {
    fn drop(&mut self) {
        let mut current = self.shared.current_run();
        // Only while it is still this run. A newer one may have started already,
        // and an old token dropping late must not end it.
        if *current == self.run {
            *current = 0;
        }
    }
}

impl Shared {
    fn current_run(&self) -> std::sync::MutexGuard<'_, u64> {
        // A poisoned lock holds a number all the same.
        self.current_run
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Askpass {
    /// Creates the socket and starts answering on it with `prompter`.
    pub fn start(prompter: Prompter) -> io::Result<Askpass> {
        Askpass::start_with_helper(prompter, &std::env::current_exe()?)
    }

    /// [`Askpass::start`] with the program that acts as the helper named
    /// explicitly. It has to be a runandlog binary; this exists for the tests,
    /// whose own executable is not one.
    pub fn start_with_helper(prompter: Prompter, program: &Path) -> io::Result<Askpass> {
        let dir = private_dir()?;
        let socket = dir.join("socket");
        let helper = dir.join(HELPER_NAME);
        let started = (|| {
            std::os::unix::fs::symlink(program, &helper)?;
            UnixListener::bind(&socket)
        })();
        let listener = match started {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(error);
            }
        };
        let shared = Arc::new(Shared {
            socket,
            helper,
            closing: AtomicBool::new(false),
            current_run: Mutex::new(0),
            next_run: AtomicU64::new(1),
        });
        let listener = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || serve(listener, &shared, prompter))
        };
        Ok(Askpass {
            dir,
            shared,
            listener: Some(listener),
        })
    }

    /// Starts a run: adds the variables that point commands at the helper, with
    /// the run's number, to `options`. The run lasts as long as the returned
    /// [`RunInProgress`] does.
    ///
    /// **Once per run, as it starts.** Each call begins a new run, which ends the
    /// one before it. Front ends hand this to the session as a [`RunHook`] (see
    /// [`Askpass::hook`]) rather than calling it themselves.
    pub fn apply(&self, options: &mut ExecOptions) -> RunInProgress {
        apply(&self.shared, options)
    }

    /// [`Askpass::apply`] as something a session can call for every run.
    pub fn hook(&self) -> RunHook {
        let shared = Arc::clone(&self.shared);
        Arc::new(move |options| Box::new(apply(&shared, options)))
    }
}

/// Starts a new run and adds its variables to `options`.
fn apply(shared: &Arc<Shared>, options: &mut ExecOptions) -> RunInProgress {
    let run = shared.next_run.fetch_add(1, Ordering::SeqCst);
    *shared.current_run() = run;
    options
        .env
        .extend(environment(shared, run, |name| std::env::var_os(name)));
    RunInProgress {
        shared: Arc::clone(shared),
        run,
    }
}

/// The variables to add, given what the environment already has.
fn environment(
    shared: &Shared,
    run: u64,
    current: impl Fn(&str) -> Option<OsString>,
) -> Vec<(OsString, OsString)> {
    let mut env = vec![
        (SOCKET_VAR.into(), shared.socket.clone().into_os_string()),
        (RUN_VAR.into(), run.to_string().into()),
    ];
    for name in HELPER_VARS {
        if current(name).is_some_and(|value| !value.is_empty()) {
            continue;
        }
        env.push((name.into(), shared.helper.clone().into_os_string()));
    }
    // Whichever askpass ssh ends up with -- ours or the user's -- it is only
    // ever going to be used if ssh is told to. Left to itself, or with `prefer`,
    // ssh turns to the askpass only when it also sees a display (readpass.c),
    // which macOS and an SSH session do not have; `never` rules it out. The
    // command has no terminal for ssh to fall back on, so with anything but
    // `force` nobody gets asked at all. Set regardless of what was inherited:
    // a value set for interactive shells, where a terminal exists, does not
    // carry the same meaning here.
    env.push(("SSH_ASKPASS_REQUIRE".into(), "force".into()));
    env
}

impl Drop for Askpass {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        // The listener is parked in `accept`, or in a prompt; a connection is what
        // wakes it from the first, and the flag is what the prompt gives up on.
        let _ = UnixStream::connect(&self.shared.socket);
        // Waited for, because the terminal prompt has the terminal's echo turned off
        // until it returns. Leaving without it would hand the user back a shell that
        // does not show what they type.
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Creates a directory only this user can enter, for the socket and the helper.
///
/// `create_dir` rather than `create_dir_all`, so that a directory someone else
/// prepared under the same name is refused instead of adopted. The name carries
/// the process id, the clock and a serial number: the clock alone is not enough
/// within one process (two [`Askpass`] started in the same tick -- the tests do
/// that, and macOS ticks in microseconds), and a name that is taken all the same
/// -- left by a process that had this id before -- is passed over for the next.
fn private_dir() -> io::Result<PathBuf> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    const ATTEMPTS: u32 = 16;

    let pid = std::process::id();
    let mut last = None;
    for _ in 0..ATTEMPTS {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0);
        let serial = SERIAL.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("runandlog-{pid}-{nanos:08x}-{serial}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free name for the askpass directory")))
}

/// Answers helpers one at a time until the [`Askpass`] is dropped.
///
/// One at a time on purpose: a front end shows one prompt, and a second command
/// asking meanwhile is better kept waiting than shown over the first.
fn serve(listener: UnixListener, shared: &Shared, prompter: Prompter) {
    for stream in listener.incoming() {
        if shared.closing.load(Ordering::SeqCst) {
            return;
        }
        // A connection that fails or sends garbage is the helper's problem, not the
        // listener's; the next one is served as usual.
        if let Ok(stream) = stream {
            let _ = answer(stream, &prompter, shared);
        }
    }
}

/// Reads one prompt and writes back the user's answer.
fn answer(mut stream: UnixStream, prompter: &Prompter, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(PROMPT_READ_TIMEOUT))?;
    // A helper that stopped reading must not be able to hold the reply up.
    stream.set_write_timeout(Some(PROMPT_POLL))?;
    let Some(message) = read_prompt(&mut stream)? else {
        return stream.write_all(REPLY_CANCEL);
    };
    // A helper of any run but the one in progress -- one a finished cell left
    // running in the background, asking during the next cell or between cells --
    // is refused without a word to the user. Answering it would put the question
    // up as the running cell's, or with no cell at all, and send what the user
    // types to a process they were not shown.
    let Some((run, prompt)) = split_run(&message) else {
        return stream.write_all(REPLY_CANCEL);
    };
    if run == 0 || run != *shared.current_run() {
        return stream.write_all(REPLY_CANCEL);
    }
    let prompt = String::from_utf8_lossy(prompt);
    let current = || *shared.current_run() == run;
    // A prompt is given up once its run has ended as well as once its helper has
    // gone: the check above cannot rule out a run that ends right after it, and a
    // question left up from the previous cell would be read as the next one's.
    //
    // The helper sends nothing after the prompt and keeps the connection open, so
    // end-of-file on it means the helper has gone. Reading is the check that works
    // the same everywhere: a write to a dead peer fails with EPIPE on Linux, but
    // macOS lets it succeed while there is buffer space, which it always is.
    let gone = || shared.closing.load(Ordering::SeqCst) || !current() || peer_has_gone(&stream);
    match prompter(prompt.trim_end(), &gone) {
        Some(secret) => {
            // Looked at once more, and held: the run may have ended between the
            // user answering and the answer getting here, and it must not end
            // between here and the write either. The write times out, so the
            // hold is short even for a helper that has stopped reading.
            let current = shared.current_run();
            if *current != run {
                drop(current);
                return stream.write_all(REPLY_CANCEL);
            }
            stream.write_all(REPLY_OK)?;
            stream.write_all(secret.as_bytes())
        }
        None => stream.write_all(REPLY_CANCEL),
    }
}

/// Takes the run number off the front of the helper's message: a line of digits,
/// then the prompt. `None` when it is not there.
fn split_run(message: &[u8]) -> Option<(u64, &[u8])> {
    let end = message.iter().position(|&byte| byte == b'\n')?;
    let run = std::str::from_utf8(&message[..end]).ok()?.parse().ok()?;
    Some((run, &message[end + 1..]))
}

/// Reads up to the [`PROMPT_END`]. `None` when the helper went away first, or sent
/// more than a prompt.
fn read_prompt(stream: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
    let mut prompt = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        let end = chunk[..read].iter().position(|&byte| byte == PROMPT_END);
        prompt.extend_from_slice(&chunk[..end.unwrap_or(read)]);
        if prompt.len() > MAX_PROMPT_BYTES {
            return Ok(None);
        }
        if end.is_some() {
            return Ok(Some(prompt));
        }
    }
}

/// Whether the helper on the other end of `stream` has closed it, without waiting
/// for it to say anything (it never does, once the prompt is sent).
fn peer_has_gone(stream: &UnixStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let mut byte = [0u8; 1];
    let gone = match (&*stream).read(&mut byte) {
        // End-of-file: the helper closed or died.
        Ok(0) => true,
        // Nothing to read is what a live helper looks like; a stray byte is odd but
        // not a departure.
        Ok(_) => false,
        Err(error) => error.kind() != io::ErrorKind::WouldBlock,
    };
    // Put back, or the reply write below would fail with WouldBlock instead of
    // waiting its allotted time.
    gone || stream.set_nonblocking(false).is_err()
}

/// A prompter that asks on the terminal runandlog was started from, with the
/// typing hidden. `None` when there is no terminal to ask on.
///
/// `abandoned` is looked at while waiting for the answer. Ctrl-C reaches runandlog
/// rather than the command, and it cancels the command -- whose helper is then gone
/// -- so a prompt still up afterwards would ask for an answer nobody can use.
pub fn terminal_prompter(abandoned: fn() -> bool) -> Option<Prompter> {
    open_terminal().ok()?;
    Some(Box::new(move |prompt, gone| {
        read_hidden(prompt, &|| abandoned() || gone())
            .ok()
            .flatten()
    }))
}

/// Waits for a front end's answer to arrive on `answer`, giving up once `gone`
/// says it is no longer wanted. A dropped sender is a decline.
///
/// For prompters that hand the question to another thread -- the TUI's, the
/// window's -- and must still look at `gone` while they wait.
pub fn wait_for_answer(
    answer: &mpsc::Receiver<Option<String>>,
    gone: &dyn Fn() -> bool,
) -> Option<String> {
    loop {
        match answer.recv_timeout(PROMPT_POLL) {
            Ok(answer) => return answer,
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if gone() {
                    return None;
                }
            }
        }
    }
}

fn open_terminal() -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
}

/// Puts the terminal's settings back when dropped, so that no way out of
/// [`read_hidden`] leaves the user typing blind.
struct RestoreTerminal {
    fd: std::os::fd::RawFd,
    saved: libc::termios,
}

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        // SAFETY: `saved` came from tcgetattr on the same descriptor, which the
        // caller keeps open for as long as this guard lives.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

/// Shows `prompt` on the terminal and reads a line without echoing it.
fn read_hidden(prompt: &str, abandoned: &dyn Fn() -> bool) -> io::Result<Option<String>> {
    use std::os::fd::AsRawFd;

    let mut terminal = open_terminal()?;
    let fd = terminal.as_raw_fd();
    // SAFETY: termios is plain data, filled in by tcgetattr before it is read.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open for the whole function.
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut hidden = saved;
    hidden.c_lflag &= !libc::ECHO;
    // SAFETY: as above.
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &hidden) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let restore = RestoreTerminal { fd, saved };

    // On a line of its own: the command's output is being printed as it arrives,
    // and may have left the cursor in the middle of one.
    write!(terminal, "\r\n{prompt} ")?;
    terminal.flush()?;
    let mut line = Vec::new();
    let answer = loop {
        if abandoned() {
            break None;
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a single valid pollfd, on a descriptor that stays open.
        if unsafe { libc::poll(&mut poll_fd, 1, PROMPT_POLL.as_millis() as libc::c_int) } <= 0 {
            continue;
        }
        let mut chunk = [0u8; 256];
        match terminal.read(&mut chunk) {
            // The terminal went away.
            Ok(0) => break None,
            Ok(read) => {
                line.extend_from_slice(&chunk[..read]);
                if let Some(end) = line.iter().position(|&byte| byte == b'\n') {
                    line.truncate(end);
                    break Some(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    };
    drop(restore);
    // The Enter that ended the line was not echoed either.
    let _ = write!(terminal, "\r\n");
    Ok(answer.map(|()| {
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8_lossy(&line).into_owned()
    }))
}

/// Whether this process was started as the helper.
pub fn started_as_helper(argv0: &std::ffi::OsStr) -> bool {
    Path::new(argv0).file_name() == Some(std::ffi::OsStr::new(HELPER_NAME))
}

/// The helper itself: forwards the prompt and prints the answer.
///
/// Exits non-zero when the user declined or nobody could be asked, which is how
/// sudo, ssh and git tell "no password" from an empty one.
pub fn helper_main(prompt: Option<String>) -> ExitCode {
    let prompt = prompt.unwrap_or_else(|| "Password:".to_string());
    match ask(&prompt) {
        Ok(Some(secret)) => {
            let mut stdout = io::stdout();
            if writeln!(stdout, "{secret}")
                .and_then(|()| stdout.flush())
                .is_ok()
            {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(None) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("runandlog-askpass: {error}");
            ExitCode::FAILURE
        }
    }
}

fn ask(prompt: &str) -> io::Result<Option<String>> {
    let socket = std::env::var_os(SOCKET_VAR)
        .ok_or_else(|| io::Error::other(format!("{SOCKET_VAR} is not set")))?;
    // Not defaulted: a helper that cannot say which run it belongs to is not
    // answered, and that is the right outcome for one started outside a run.
    let run =
        std::env::var(RUN_VAR).map_err(|_| io::Error::other(format!("{RUN_VAR} is not set")))?;
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(run.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.write_all(prompt.as_bytes())?;
    stream.write_all(&[PROMPT_END])?;
    // The write side stays open on purpose: the listener reads end-of-file on it
    // as "the helper has gone" and drops the prompt. The listener closes its end
    // once it has answered, which is what ends the read below.
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    match reply.strip_prefix(REPLY_OK) {
        Some(secret) => Ok(Some(String::from_utf8_lossy(secret).into_owned())),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc;

    /// Starts a run, as a front end's session does before each command, and
    /// returns its number and the token that keeps it going.
    fn start_run(askpass: &Askpass) -> (u64, RunInProgress) {
        let mut options = ExecOptions::new(std::env::temp_dir());
        let run = askpass.apply(&mut options);
        (run.run, run)
    }

    /// What the helper sends: the run, a newline, the prompt, the terminator.
    fn message(run: u64, prompt: &str) -> Vec<u8> {
        let mut message = format!("{run}\n{prompt}").into_bytes();
        message.push(PROMPT_END);
        message
    }

    /// Connects as a helper of `run` would and sends `prompt`. The connection is
    /// left open, as the helper leaves it.
    fn helper(askpass: &Askpass, run: u64, prompt: &str) -> UnixStream {
        let mut stream = UnixStream::connect(&askpass.shared.socket).unwrap();
        stream.write_all(&message(run, prompt)).unwrap();
        stream
    }

    fn reply_of(mut stream: UnixStream) -> Vec<u8> {
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        reply
    }

    #[test]
    fn a_prompt_goes_to_the_front_end_and_the_answer_comes_back() {
        let (asked_tx, asked_rx) = mpsc::channel();
        let asked_tx = Mutex::new(asked_tx);
        let askpass = Askpass::start(Box::new(move |prompt, _| {
            asked_tx.lock().unwrap().send(prompt.to_string()).unwrap();
            Some("s3cret".to_string())
        }))
        .unwrap();

        let (run, _run) = start_run(&askpass);
        let mut stream = helper(&askpass, run, "[sudo] password for me: ");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();

        assert_eq!(asked_rx.recv().unwrap(), "[sudo] password for me:");
        assert_eq!(reply, b"OK\ns3cret");
    }

    #[test]
    fn a_declined_prompt_is_told_apart_from_an_empty_password() {
        let askpass = Askpass::start(Box::new(|_, _| None)).unwrap();
        let (run, _run) = start_run(&askpass);
        assert_eq!(reply_of(helper(&askpass, run, "Password:")), REPLY_CANCEL);
    }

    #[test]
    fn only_the_current_run_is_answered() {
        let asked = Arc::new(AtomicBool::new(false));
        let askpass = Askpass::start({
            let asked = Arc::clone(&asked);
            Box::new(move |_, _| {
                asked.store(true, Ordering::SeqCst);
                Some("s3cret".to_string())
            })
        })
        .unwrap();

        // Before any run: nothing to attribute a question to.
        assert_eq!(reply_of(helper(&askpass, 0, "Password:")), REPLY_CANCEL);
        assert_eq!(reply_of(helper(&askpass, 1, "Password:")), REPLY_CANCEL);
        // A message with no run at all.
        let mut stream = UnixStream::connect(&askpass.shared.socket).unwrap();
        stream.write_all(b"Password:\0").unwrap();
        assert_eq!(reply_of(stream), REPLY_CANCEL);
        assert!(!asked.load(Ordering::SeqCst));

        let (first, first_run) = start_run(&askpass);
        assert_eq!(
            reply_of(helper(&askpass, first, "Password:")),
            b"OK\ns3cret"
        );
        asked.store(false, Ordering::SeqCst);

        // The first cell's command ends. A process it left behind still has its
        // number, and there is no cell to show a question under.
        drop(first_run);
        assert_eq!(reply_of(helper(&askpass, first, "Password:")), REPLY_CANCEL);
        assert!(!asked.load(Ordering::SeqCst));

        // The next cell starts. The leftover is not the one the user is looking at.
        let (second, _second_run) = start_run(&askpass);
        assert_ne!(first, second);
        assert_eq!(reply_of(helper(&askpass, first, "Password:")), REPLY_CANCEL);
        assert!(!asked.load(Ordering::SeqCst));
        assert_eq!(
            reply_of(helper(&askpass, second, "Password:")),
            b"OK\ns3cret"
        );
    }

    #[test]
    fn a_prompt_still_up_when_its_run_ends_is_given_up() {
        // The run check on arrival cannot see a run that ends right after it. A
        // question that is up when the run ends has to come down, or it is taken
        // for the next cell's.
        let (asked_tx, asked_rx) = mpsc::channel();
        let asked_tx = Mutex::new(asked_tx);
        let askpass = Askpass::start(Box::new(move |_, gone| {
            asked_tx.lock().unwrap().send(()).unwrap();
            let (_keep, answer) = mpsc::channel();
            wait_for_answer(&answer, gone)
        }))
        .unwrap();

        let (run, in_progress) = start_run(&askpass);
        let stream = helper(&askpass, run, "Password:");
        asked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(in_progress);
        // The helper is still connected; only the run ending can have done this.
        assert_eq!(reply_of(stream), REPLY_CANCEL);
    }

    #[test]
    fn an_answer_arriving_after_the_run_ended_is_not_delivered() {
        // The user answered the previous cell's question just as the next cell
        // started. The answer is not the next cell's to receive, and the helper
        // that asked belongs to a run that is over.
        let (asked_tx, asked_rx) = mpsc::channel();
        let asked_tx = Mutex::new(asked_tx);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let askpass = Askpass::start(Box::new(move |_, _| {
            asked_tx.lock().unwrap().send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Some("too late".to_string())
        }))
        .unwrap();

        let (run, in_progress) = start_run(&askpass);
        let stream = helper(&askpass, run, "Password:");
        asked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The next cell has started by the time the answer comes.
        drop(in_progress);
        let (_, _next) = start_run(&askpass);
        release_tx.send(()).unwrap();
        assert_eq!(reply_of(stream), REPLY_CANCEL);
    }

    #[test]
    fn the_hook_starts_a_run_like_apply_does() {
        let askpass = Askpass::start(Box::new(|_, _| None)).unwrap();
        let current = || *askpass.shared.current_run();
        let hook = askpass.hook();
        let mut options = ExecOptions::new(std::env::temp_dir());
        let first = hook(&mut options);
        assert_eq!(current(), 1);
        let value = options
            .env
            .iter()
            .find(|(key, _)| key == RUN_VAR)
            .map(|(_, value)| value.to_string_lossy().into_owned());
        assert_eq!(value.as_deref(), Some("1"));
        // Between runs there is no run.
        drop(first);
        assert_eq!(current(), 0);
        // A token dropped late does not end the run that came after it.
        let first = hook(&mut options);
        let second = hook(&mut options);
        assert_eq!(current(), 3);
        drop(first);
        assert_eq!(current(), 3);
        drop(second);
        assert_eq!(current(), 0);
    }

    #[test]
    fn a_prompt_longer_than_a_prompt_can_be_is_refused() {
        let asked = Arc::new(AtomicBool::new(false));
        let askpass = Askpass::start({
            let asked = Arc::clone(&asked);
            Box::new(move |_, _| {
                asked.store(true, Ordering::SeqCst);
                Some("never sent".to_string())
            })
        })
        .unwrap();
        let (run, _run) = start_run(&askpass);
        // The limit is on the whole message, run number included.
        let head = format!("{run}\n");
        let exchange = |length: usize| {
            let prompt = "x".repeat(length - head.len());
            reply_of(helper(&askpass, run, &prompt))
        };
        // The longest message allowed, and one byte more. The limit has to hold
        // wherever the reads happen to split, so the extra byte arrives in the
        // same chunk as the terminator.
        assert_eq!(exchange(MAX_PROMPT_BYTES), b"OK\nnever sent");
        asked.store(false, Ordering::SeqCst);
        assert_eq!(exchange(MAX_PROMPT_BYTES + 1), REPLY_CANCEL);
        assert!(!asked.load(Ordering::SeqCst));
    }

    #[test]
    fn a_helper_that_leaves_before_finishing_its_prompt_is_not_asked_for() {
        let asked = Arc::new(AtomicBool::new(false));
        let askpass = Askpass::start({
            let asked = Arc::clone(&asked);
            Box::new(move |_, _| {
                asked.store(true, Ordering::SeqCst);
                None
            })
        })
        .unwrap();
        // No PROMPT_END: the helper died mid-sentence (or is not our helper).
        let (run, _run) = start_run(&askpass);
        let mut stream = UnixStream::connect(&askpass.shared.socket).unwrap();
        stream
            .write_all(format!("{run}\nPassword:").as_bytes())
            .unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        // The reply may not arrive at all: a peer that has closed its writing side
        // before the listener accepted is reported closed outright on some systems.
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply);
        assert!(reply.is_empty() || reply == REPLY_CANCEL);
        assert!(!asked.load(Ordering::SeqCst));
    }

    #[test]
    fn the_directory_is_private_and_removed_afterwards() {
        use std::os::unix::fs::PermissionsExt;

        let askpass = Askpass::start(Box::new(|_, _| None)).unwrap();
        let dir = askpass.dir.clone();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        drop(askpass);
        assert!(!dir.exists());
    }

    #[test]
    fn askpasses_started_in_the_same_tick_get_directories_of_their_own() {
        // The clock is not fine enough to tell them apart on every system: macOS
        // ticks in microseconds, and this failed there once in CI.
        let started: Vec<_> = (0..8)
            .map(|_| thread::spawn(|| Askpass::start(Box::new(|_, _| None)).unwrap()))
            .collect();
        let askpasses: Vec<_> = started
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let mut dirs: Vec<_> = askpasses.iter().map(|a| a.dir.clone()).collect();
        dirs.sort();
        dirs.dedup();
        assert_eq!(dirs.len(), askpasses.len());
    }

    #[test]
    fn an_askpass_the_user_chose_is_kept() {
        let askpass = Askpass::start(Box::new(|_, _| None)).unwrap();
        let env = environment(&askpass.shared, 1, |name| {
            (name == "SUDO_ASKPASS").then(|| OsString::from("/usr/bin/my-askpass"))
        });
        let names: Vec<_> = env
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert!(!names.contains(&"SUDO_ASKPASS".to_string()));
        assert!(names.contains(&"SSH_ASKPASS".to_string()));
        assert!(names.contains(&"SSH_ASKPASS_REQUIRE".to_string()));
        assert!(names.contains(&"GIT_ASKPASS".to_string()));
        assert!(names.contains(&SOCKET_VAR.to_string()));
        assert!(names.contains(&RUN_VAR.to_string()));
    }

    #[test]
    fn ssh_is_made_to_use_the_askpass_whichever_one_it_is() {
        // The user's own SSH_ASKPASS is kept, but without `force` ssh would not
        // call it either: there is no display here, and no terminal.
        let askpass = Askpass::start(Box::new(|_, _| None)).unwrap();
        let env = environment(&askpass.shared, 1, |name| match name {
            "SSH_ASKPASS" => Some(OsString::from("/usr/bin/my-askpass")),
            "SSH_ASKPASS_REQUIRE" => Some(OsString::from("prefer")),
            _ => None,
        });
        let value = |wanted: &str| {
            env.iter()
                .find(|(key, _)| key == wanted)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(value("SSH_ASKPASS"), None);
        assert_eq!(value("SSH_ASKPASS_REQUIRE").as_deref(), Some("force"));
    }

    #[test]
    fn a_prompt_is_given_up_once_the_helper_has_gone() {
        // The command that asked can end with the question still up -- a timeout
        // kills it, say. The prompt has to notice, or the terminal prompt keeps the
        // terminal's echo off and dropping the Askpass waits on it forever.
        let (asked_tx, asked_rx) = mpsc::channel();
        let asked_tx = Mutex::new(asked_tx);
        let (gave_up_tx, gave_up_rx) = mpsc::channel();
        let gave_up_tx = Mutex::new(gave_up_tx);
        let askpass = Askpass::start(Box::new(move |_, gone| {
            asked_tx.lock().unwrap().send(()).unwrap();
            let started = std::time::Instant::now();
            while !gone() {
                if started.elapsed() > Duration::from_secs(10) {
                    return None;
                }
                thread::sleep(PROMPT_POLL);
            }
            gave_up_tx.lock().unwrap().send(()).unwrap();
            None
        }))
        .unwrap();

        let (run, _run) = start_run(&askpass);
        let stream = helper(&askpass, run, "Password:");
        // Only once the question is up. A helper that closes before the listener
        // has accepted it is not a prompt that has to be given up; on macOS such
        // a connection is reported closed and never reaches the prompter at all.
        asked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(stream);

        assert!(gave_up_rx.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn dropping_waits_for_a_prompt_to_give_up() {
        let (asked_tx, asked_rx) = mpsc::channel();
        let asked_tx = Mutex::new(asked_tx);
        let askpass = Askpass::start(Box::new(move |_, gone| {
            asked_tx.lock().unwrap().send(()).unwrap();
            let (_keep, answer) = mpsc::channel();
            wait_for_answer(&answer, gone)
        }))
        .unwrap();

        // A helper that stays connected: only the drop can end the prompt.
        let (run, _run) = start_run(&askpass);
        let stream = helper(&askpass, run, "Password:");
        asked_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let started = std::time::Instant::now();
        drop(askpass);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(reply_of(stream), REPLY_CANCEL);
    }

    #[test]
    fn the_helper_is_recognised_by_its_name() {
        assert!(started_as_helper(std::ffi::OsStr::new(
            "/tmp/x/runandlog-askpass"
        )));
        assert!(started_as_helper(std::ffi::OsStr::new("runandlog-askpass")));
        assert!(!started_as_helper(std::ffi::OsStr::new(
            "/usr/bin/runandlog"
        )));
    }
}
