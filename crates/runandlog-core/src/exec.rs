//! コマンドの実行。
//!
//! stdout と stderr は 1 本のパイプにまとめて受け取る。順序を保ったまま 1 つのログとして
//! Markdown に書き戻したいため、別々に読んで連結する方式は採らない。

use std::io::{Read, pipe};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};

/// 実行の待ち合わせ間隔。
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// プロセス終了後、出力の読み取りスレッドを待つ猶予。
///
/// タイムアウト無しで起動されたバックグラウンドの孫プロセスがパイプを掴んだままだと
/// EOF が来ないため、ここで打ち切って読めた分だけを結果とする。
const DRAIN_GRACE: Duration = Duration::from_millis(300);
/// 取り込む出力の既定の上限。無限に出力し続けるコマンドでメモリを使い切らないための保険。
const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// 実行時の設定。
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// コマンドを渡すシェル。
    pub shell: PathBuf,
    /// 作業ディレクトリ。
    pub cwd: PathBuf,
    /// 実行の打ち切り時間。`None` なら無制限。
    pub timeout: Option<Duration>,
    /// 取り込む出力の上限バイト数。超えた分は捨てて `truncated` を立てる。
    pub max_output_bytes: usize,
}

impl ExecOptions {
    /// `cwd` を指定し、シェルは環境変数 `SHELL` (無ければ `/bin/sh`) を使う。
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

/// 1 回の実行結果。
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    /// 実行開始時刻 (ローカルタイム)。
    pub started_at: DateTime<Local>,
    /// 実行にかかった時間。
    pub duration: Duration,
    /// 終了コード。シグナルで終了した場合は `None`。
    pub exit_code: Option<i32>,
    /// stdout と stderr をまとめた出力。
    pub output: String,
    /// タイムアウトで打ち切ったか。
    pub timed_out: bool,
    /// 出力が上限を超えて打ち切られたか。
    pub truncated: bool,
}

impl ExecOutcome {
    /// 成功したとみなせるか。
    pub fn is_success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    /// 終了状態の短い説明。
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

/// `command` をシェル経由で実行する。
///
/// シェルの起動に失敗した場合だけ `Err` を返す。コマンド自体が失敗した場合は
/// 終了コードを持つ `ExecOutcome` として返す。
pub fn run(command: &str, options: &ExecOptions) -> std::io::Result<ExecOutcome> {
    // 存在しないディレクトリのまま起動すると、シェルの起動失敗としか分からなくなる。
    if !options.cwd.is_dir() {
        return Err(std::io::Error::other(format!(
            "作業ディレクトリがありません: {}",
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
    // タイムアウト時にシェルの子孫までまとめて終了させるため、専用のプロセスグループに入れる。
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut builder, 0);
    let child = builder.spawn()?;

    // 読み取りスレッドは EOF まで読み続ける。孫プロセスがパイプを保持していると
    // 終わらないため、結果はロック越しに共有し、本体は待たずに進めるようにする。
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let truncated = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (drained_tx, drained_rx) = mpsc::channel();
    let reader_buffer = Arc::clone(&buffer);
    let reader_truncated = Arc::clone(&truncated);
    let reader_stop = Arc::clone(&stop);
    let limit = options.max_output_bytes;
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 || reader_stop.load(Ordering::Relaxed) {
                break;
            }
            let mut buffer = reader_buffer.lock().unwrap();
            let room = limit.saturating_sub(buffer.len());
            if room == 0 {
                // 読み捨てる。読むのをやめるとパイプが詰まり、コマンドが止まってしまう。
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

    if drained_rx.recv_timeout(DRAIN_GRACE).is_err() {
        // 孫プロセスがパイプを掴んだまま出力を続けている。読み取りスレッドが延々と
        // 動き続けないよう、次に読めた時点で止まるように伝える。
        stop.store(true, Ordering::Relaxed);
    }
    let output = String::from_utf8_lossy(&buffer.lock().unwrap()).to_string();

    Ok(ExecOutcome {
        started_at,
        duration: start.elapsed(),
        exit_code,
        output,
        timed_out,
        truncated: truncated.load(Ordering::Relaxed),
    })
}

/// シェルとその子孫をまとめて終了させる。
///
/// シェルだけを kill すると、シェルが起動したコマンドが実行を続け、パイプも開いたまま残る。
#[cfg(unix)]
fn kill_process_group(child: &mut Child) {
    // process_group(0) で起動しているので、プロセスグループ ID は子プロセスの PID と同じ。
    let pid = child.id() as i32;
    // SAFETY: 自分が起動したプロセスグループにシグナルを送るだけ。
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
        assert!(error.to_string().contains("作業ディレクトリ"));
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
        // 孫プロセスがパイプを掴んだままだと、シェルだけを kill しても EOF が来ない。
        let outcome = run("sleep 30 & wait", &opts).unwrap();
        assert!(outcome.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
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
