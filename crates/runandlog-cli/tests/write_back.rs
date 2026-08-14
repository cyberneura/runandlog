//! Markdown への書き戻しを実ファイルで確認する。

use std::path::{Path, PathBuf};

use runandlog_cli::session::Session;
use runandlog_core::ExecOptions;

/// テスト用の一時ディレクトリ。後始末までを持つ。
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let dir =
            std::env::temp_dir().join(format!("runandlog-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join(name)).unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn session(path: &Path, max_inline_lines: usize) -> Session {
    let mut options = ExecOptions::new(path.parent().unwrap());
    options.shell = PathBuf::from("/bin/sh");
    options.timeout = Some(std::time::Duration::from_secs(30));
    Session::load(path, options, max_inline_lines).unwrap()
}

#[test]
fn writes_the_result_back_into_the_markdown() {
    let dir = TempDir::new("writeback");
    let path = dir.write("note.md", "# note\n\n```shell\necho hello\n```\n\ntail\n");
    let mut session = session(&path, 50);

    let outcome = session.run_cell(0).unwrap();
    assert!(outcome.is_success());

    let text = dir.read("note.md");
    assert!(text.contains("<!-- runandlog:begin -->"));
    assert!(text.contains("```text\nhello\n```"));
    assert!(text.ends_with("tail\n"));
}

#[test]
fn rerunning_replaces_the_previous_result_instead_of_appending() {
    let dir = TempDir::new("rerun");
    let path = dir.write("note.md", "```shell\necho hello\n```\n");
    let mut session = session(&path, 50);

    session.run_cell(0).unwrap();
    session.run_cell(0).unwrap();
    session.run_cell(0).unwrap();

    let text = dir.read("note.md");
    assert_eq!(text.matches("<!-- runandlog:begin -->").count(), 1);
    assert_eq!(text.matches("<!-- runandlog:end -->").count(), 1);
    assert_eq!(
        text.matches("hello").count(),
        2,
        "コマンドと結果で 1 回ずつ"
    );
}

#[test]
fn long_output_goes_to_a_sidecar_file() {
    let dir = TempDir::new("sidecar");
    let path = dir.write("note.md", "```shell\nseq 1 12\n```\n");
    let mut session = session(&path, 5);

    session.run_cell(0).unwrap();

    let text = dir.read("note.md");
    assert!(text.contains("[note-result-1.txt](note-result-1.txt)"));
    assert_eq!(
        dir.read("note-result-1.txt"),
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n"
    );
}

#[test]
fn designated_out_file_is_used_and_not_duplicated_in_the_result_block() {
    let dir = TempDir::new("designated");
    let path = dir.write(
        "note.md",
        "```shell\necho designated\n```\n\nResult:\n[out.txt](out.txt)\n",
    );
    let mut session = session(&path, 50);

    session.run_cell(0).unwrap();

    let text = dir.read("note.md");
    assert_eq!(dir.read("out.txt"), "designated\n");
    assert_eq!(text.matches("[out.txt](out.txt)").count(), 1);
}

#[test]
fn each_cell_keeps_its_own_result() {
    let dir = TempDir::new("multi");
    let path = dir.write(
        "note.md",
        "```shell\necho first\n```\n\n```shell\necho second\n```\n",
    );
    let mut session = session(&path, 50);

    session.run_cell(1).unwrap();
    session.run_cell(0).unwrap();

    let text = dir.read("note.md");
    assert_eq!(text.matches("<!-- runandlog:begin -->").count(), 2);
    let first = text.find("echo first").unwrap();
    let second = text.find("echo second").unwrap();
    let first_result = text.find("```text\nfirst\n```").unwrap();
    let second_result = text.find("```text\nsecond\n```").unwrap();
    assert!(first < first_result && first_result < second);
    assert!(second < second_result);
}

#[test]
fn an_external_edit_that_keeps_the_cell_is_preserved() {
    let dir = TempDir::new("external-keep");
    let path = dir.write("note.md", "```shell\necho hello\n```\n");
    let mut session = session(&path, 50);

    // 実行中に人が本文を足した状況を作る。
    dir.write(
        "note.md",
        "# 追記された見出し\n\n```shell\necho hello\n```\n",
    );
    session.run_cell(0).unwrap();

    let text = dir.read("note.md");
    assert!(text.contains("# 追記された見出し"));
    assert!(text.contains("```text\nhello\n```"));
}

#[test]
fn an_external_edit_that_changes_the_cell_is_reported_instead_of_overwritten() {
    let dir = TempDir::new("external-change");
    let path = dir.write("note.md", "```shell\necho hello\n```\n");
    let mut session = session(&path, 50);

    dir.write("note.md", "```shell\necho something-else\n```\n");
    let error = session.run_cell(0).unwrap_err();

    assert!(error.to_string().contains("書き換えられた"));
    assert_eq!(dir.read("note.md"), "```shell\necho something-else\n```\n");
}

#[test]
fn an_out_file_outside_the_markdown_directory_is_ignored() {
    let dir = TempDir::new("escape");
    let path = dir.write("note.md", "```shell out=../escaped.txt\necho nope\n```\n");
    let mut session = session(&path, 50);

    session.run_cell(0).unwrap();

    assert!(!dir.0.parent().unwrap().join("escaped.txt").exists());
    let text = dir.read("note.md");
    assert!(text.contains("```text\nnope\n```"));
}

#[cfg(unix)]
#[test]
fn an_out_file_reached_through_a_symlinked_directory_is_ignored() {
    let dir = TempDir::new("symlink-escape");
    let outside = TempDir::new("symlink-target");
    std::os::unix::fs::symlink(&outside.0, dir.0.join("logs")).unwrap();
    let path = dir.write("note.md", "```shell out=logs/escaped.txt\necho nope\n```\n");
    let mut session = session(&path, 50);

    session.run_cell(0).unwrap();

    assert!(!outside.0.join("escaped.txt").exists());
    assert!(dir.read("note.md").contains("```text\nnope\n```"));
}

#[test]
fn a_failing_command_is_recorded_with_its_exit_code() {
    let dir = TempDir::new("failure");
    let path = dir.write("note.md", "```shell\necho boom 1>&2\nexit 7\n```\n");
    let mut session = session(&path, 50);

    let outcome = session.run_cell(0).unwrap();
    assert!(!outcome.is_success());

    let text = dir.read("note.md");
    assert!(text.contains("(exit 7,"));
    assert!(text.contains("boom"));
}
