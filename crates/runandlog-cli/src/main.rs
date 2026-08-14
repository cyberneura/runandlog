//! Run and Log の CLI / TUI エントリーポイント。

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use runandlog_cli::session::Session;
use runandlog_cli::tui;
use runandlog_core::ExecOptions;

/// Markdown に書かれたシェルコマンドを実行し、結果を同じ Markdown に書き戻す。
#[derive(Parser, Debug)]
#[command(name = "runandlog", version, about, long_about = None)]
struct Args {
    /// 対象の Markdown ファイル。
    file: PathBuf,

    /// デスクトップアプリ (GUI) で開く。
    #[arg(short, long)]
    gui: bool,

    /// セルの一覧を表示して終了する。
    #[arg(short, long)]
    list: bool,

    /// 指定した番号のセルだけを実行する (1 始まり)。繰り返し指定できる。
    #[arg(short, long, value_name = "N")]
    run: Vec<usize>,

    /// すべてのセルを順に実行する。
    #[arg(short = 'a', long)]
    run_all: bool,

    /// 出力がこの行数を超えたら別ファイルに書き出す。
    #[arg(long, value_name = "N", default_value_t = 50)]
    max_inline_lines: usize,

    /// コマンドを渡すシェル。既定は環境変数 SHELL。
    #[arg(long, value_name = "PATH")]
    shell: Option<PathBuf>,

    /// コマンドの作業ディレクトリ。既定は Markdown ファイルのあるディレクトリ。
    #[arg(long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// 1 セルあたりの実行時間の上限 (秒)。既定は無制限。
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
        eprintln!(
            "runandlog: GUI はまだ実装されていません。TUI で開くには --gui を外してください。"
        );
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

/// 実行対象のセル番号 (0 始まり) を決める。空なら TUI を開く。
fn targets(args: &Args, session: &Session) -> std::io::Result<Vec<usize>> {
    if args.run_all {
        return Ok((0..session.len()).collect());
    }
    let mut targets = Vec::new();
    for number in &args.run {
        if *number == 0 || *number > session.len() {
            return Err(std::io::Error::other(format!(
                "セル {number} は存在しません (セル数: {})",
                session.len()
            )));
        }
        targets.push(number - 1);
    }
    Ok(targets)
}

fn print_list(session: &Session) {
    if session.is_empty() {
        println!("実行できるセルがありません: {}", session.path().display());
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
