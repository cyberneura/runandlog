//! Verifies the write-back to Markdown against real files.

use std::path::{Path, PathBuf};

use runandlog_cli::session::Session;
use runandlog_core::ExecOptions;

/// A temporary directory for tests, including its own cleanup.
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
        "once in the command and once in the result"
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

    // Simulate someone adding to the body while the command runs.
    dir.write(
        "note.md",
        "# An appended heading\n\n```shell\necho hello\n```\n",
    );
    session.run_cell(0).unwrap();

    let text = dir.read("note.md");
    assert!(text.contains("# An appended heading"));
    assert!(text.contains("```text\nhello\n```"));
}

#[test]
fn an_external_edit_that_changes_the_cell_is_reported_instead_of_overwritten() {
    let dir = TempDir::new("external-change");
    let path = dir.write("note.md", "```shell\necho hello\n```\n");
    let mut session = session(&path, 50);

    dir.write("note.md", "```shell\necho something-else\n```\n");
    let error = session.run_cell(0).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("changed while the command was running")
    );
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
fn an_out_file_that_is_an_existing_directory_is_ignored() {
    let dir = TempDir::new("dir-target");
    std::fs::create_dir(dir.0.join("logs")).unwrap();
    let path = dir.write("note.md", "```shell out=logs\necho nope\n```\n");
    let mut session = session(&path, 50);

    // Writing into a directory cannot work: the rename would fail and the result
    // would be lost. The designation is ignored and the output stays inline.
    session.run_cell(0).unwrap();

    assert!(dir.0.join("logs").is_dir());
    let text = dir.read("note.md");
    assert!(text.contains("```text\nnope\n```"));
    assert!(text.contains("was ignored"));
}

#[test]
fn an_out_file_naming_the_markdown_directory_itself_is_ignored() {
    let dir = TempDir::new("dot-target");
    let path = dir.write("note.md", "```shell out=.\necho nope\n```\n");
    let mut session = session(&path, 50);

    session.run_cell(0).unwrap();

    assert!(dir.read("note.md").contains("```text\nnope\n```"));
}

#[test]
fn a_designated_out_file_with_spaces_gets_a_usable_link() {
    let dir = TempDir::new("spaced");
    let path = dir.write(
        "note.md",
        "```shell out=\"date result.txt\"\necho spaced\n```\n",
    );
    let mut session = session(&path, 50);

    session.run_cell(0).unwrap();

    assert_eq!(dir.read("date result.txt"), "spaced\n");
    // A bare destination cannot contain spaces, so the angle bracket form is used.
    assert!(
        dir.read("note.md")
            .contains("[date result.txt](<date result.txt>)")
    );
}

#[test]
fn an_out_file_naming_the_markdown_itself_is_ignored() {
    let dir = TempDir::new("self-target");
    let path = dir.write("note.md", "```shell out=note.md\necho nope\n```\n");
    let mut session = session(&path, 50);

    // Writing the output over the document would destroy it: the sidecar is written
    // first and the document second, so a failure in between leaves note.md as raw
    // command output.
    session.run_cell(0).unwrap();

    let text = dir.read("note.md");
    assert!(text.contains("```shell out=note.md"), "the cell survived");
    assert!(text.contains("```text\nnope\n```"));
    assert!(text.contains("was ignored"));
}

#[cfg(unix)]
#[test]
fn an_out_file_symlinked_to_the_markdown_itself_is_ignored() {
    let dir = TempDir::new("self-symlink");
    let path = dir.write("note.md", "```shell out=alias.md\necho nope\n```\n");
    std::os::unix::fs::symlink(&path, dir.0.join("alias.md")).unwrap();
    let mut session = session(&path, 50);

    session.run_cell(0).unwrap();

    let text = dir.read("note.md");
    assert!(text.contains("```shell out=alias.md"), "the cell survived");
    assert!(text.contains("```text\nnope\n```"));
}

#[test]
fn a_result_link_written_in_the_angle_bracket_form_round_trips() {
    let dir = TempDir::new("angle-round-trip");
    let path = dir.write(
        "note.md",
        "```shell\necho spaced\n```\n\nResult:\n[date result.txt](<date result.txt>)\n",
    );
    let mut session = session(&path, 50);

    session.run_cell(0).unwrap();

    // The angle brackets are delimiters, so the file must not be named with them.
    assert_eq!(dir.read("date result.txt"), "spaced\n");
    assert!(!dir.0.join("<date result.txt>").exists());
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
