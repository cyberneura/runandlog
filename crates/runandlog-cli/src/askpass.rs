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

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runandlog_core::ExecOptions;

/// File name the helper is started under. `main` checks for it before parsing
/// arguments, since the helper is called with a prompt, not with a Markdown file.
pub const HELPER_NAME: &str = "runandlog-askpass";
/// Variable through which the helper finds the socket.
const SOCKET_VAR: &str = "RUNANDLOG_ASKPASS_SOCKET";
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
    socket: PathBuf,
    helper: PathBuf,
    closing: Arc<AtomicBool>,
    listener: Option<thread::JoinHandle<()>>,
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
        let closing = Arc::new(AtomicBool::new(false));
        let listener = {
            let closing = Arc::clone(&closing);
            thread::spawn(move || serve(listener, &closing, prompter))
        };
        Ok(Askpass {
            dir,
            socket,
            helper,
            closing,
            listener: Some(listener),
        })
    }

    /// Adds the variables that point commands at the helper.
    pub fn apply(&self, options: &mut ExecOptions) {
        options
            .env
            .extend(self.environment(|name| std::env::var_os(name)));
    }

    /// The variables to add, given what the environment already has.
    fn environment(&self, current: impl Fn(&str) -> Option<OsString>) -> Vec<(OsString, OsString)> {
        let mut env = vec![(SOCKET_VAR.into(), self.socket.clone().into_os_string())];
        for name in HELPER_VARS {
            if current(name).is_some_and(|value| !value.is_empty()) {
                continue;
            }
            env.push((name.into(), self.helper.clone().into_os_string()));
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
}

impl Drop for Askpass {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        // The listener is parked in `accept`, or in a prompt; a connection is what
        // wakes it from the first, and the flag is what the prompt gives up on.
        let _ = UnixStream::connect(&self.socket);
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
/// prepared under the same name is refused instead of adopted.
fn private_dir() -> io::Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("runandlog-{}-{nanos:08x}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

/// Answers helpers one at a time until the [`Askpass`] is dropped.
///
/// One at a time on purpose: a front end shows one prompt, and a second command
/// asking meanwhile is better kept waiting than shown over the first.
fn serve(listener: UnixListener, closing: &AtomicBool, prompter: Prompter) {
    for stream in listener.incoming() {
        if closing.load(Ordering::SeqCst) {
            return;
        }
        // A connection that fails or sends garbage is the helper's problem, not the
        // listener's; the next one is served as usual.
        if let Ok(stream) = stream {
            let _ = answer(stream, &prompter, closing);
        }
    }
}

/// Reads one prompt and writes back the user's answer.
fn answer(mut stream: UnixStream, prompter: &Prompter, closing: &AtomicBool) -> io::Result<()> {
    stream.set_read_timeout(Some(PROMPT_READ_TIMEOUT))?;
    // A helper that stopped reading must not be able to hold the reply up.
    stream.set_write_timeout(Some(PROMPT_POLL))?;
    let Some(prompt) = read_prompt(&mut stream)? else {
        return stream.write_all(REPLY_CANCEL);
    };
    let prompt = String::from_utf8_lossy(&prompt);
    // The helper sends nothing after the prompt and keeps the connection open, so
    // end-of-file on it means the helper has gone. Reading is the check that works
    // the same everywhere: a write to a dead peer fails with EPIPE on Linux, but
    // macOS lets it succeed while there is buffer space, which it always is.
    let gone = || closing.load(Ordering::SeqCst) || peer_has_gone(&stream);
    match prompter(prompt.trim_end(), &gone) {
        Some(secret) => {
            stream.write_all(REPLY_OK)?;
            stream.write_all(secret.as_bytes())
        }
        None => stream.write_all(REPLY_CANCEL),
    }
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
    let mut stream = UnixStream::connect(socket)?;
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

    #[test]
    fn a_prompt_goes_to_the_front_end_and_the_answer_comes_back() {
        let (asked_tx, asked_rx) = mpsc::channel();
        let asked_tx = Mutex::new(asked_tx);
        let askpass = Askpass::start(Box::new(move |prompt, _| {
            asked_tx.lock().unwrap().send(prompt.to_string()).unwrap();
            Some("s3cret".to_string())
        }))
        .unwrap();

        let mut stream = UnixStream::connect(&askpass.socket).unwrap();
        stream.write_all(b"[sudo] password for me: \0").unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();

        assert_eq!(asked_rx.recv().unwrap(), "[sudo] password for me:");
        assert_eq!(reply, b"OK\ns3cret");
    }

    #[test]
    fn a_declined_prompt_is_told_apart_from_an_empty_password() {
        let askpass = Askpass::start(Box::new(|_, _| None)).unwrap();
        let mut stream = UnixStream::connect(&askpass.socket).unwrap();
        stream.write_all(b"Password:\0").unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, REPLY_CANCEL);
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
        let exchange = |length: usize| {
            let mut stream = UnixStream::connect(&askpass.socket).unwrap();
            stream.write_all(&vec![b'x'; length]).unwrap();
            stream.write_all(&[PROMPT_END]).unwrap();
            let mut reply = Vec::new();
            stream.read_to_end(&mut reply).unwrap();
            reply
        };
        // The longest prompt allowed, and one byte more. The limit has to hold
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
        let mut stream = UnixStream::connect(&askpass.socket).unwrap();
        stream.write_all(b"Password:").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, REPLY_CANCEL);
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
    fn an_askpass_the_user_chose_is_kept() {
        let askpass = Askpass::start(Box::new(|_, _| None)).unwrap();
        let env = askpass.environment(|name| {
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
    }

    #[test]
    fn ssh_is_made_to_use_the_askpass_whichever_one_it_is() {
        // The user's own SSH_ASKPASS is kept, but without `force` ssh would not
        // call it either: there is no display here, and no terminal.
        let askpass = Askpass::start(Box::new(|_, _| None)).unwrap();
        let env = askpass.environment(|name| match name {
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
        let (gave_up_tx, gave_up_rx) = mpsc::channel();
        let gave_up_tx = Mutex::new(gave_up_tx);
        let askpass = Askpass::start(Box::new(move |_, gone| {
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

        let mut stream = UnixStream::connect(&askpass.socket).unwrap();
        stream.write_all(b"Password:\0").unwrap();
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
        let mut stream = UnixStream::connect(&askpass.socket).unwrap();
        stream.write_all(b"Password:\0").unwrap();
        asked_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let started = std::time::Instant::now();
        drop(askpass);
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, REPLY_CANCEL);
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
