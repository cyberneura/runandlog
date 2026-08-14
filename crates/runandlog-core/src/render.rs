//! Formats a run result into the shape that gets written back to Markdown.

use std::path::{Component, Path, PathBuf};

use crate::exec::ExecOutcome;
use crate::parse::{BEGIN_MARKER, Cell, END_MARKER};

/// The context needed to format a result.
#[derive(Debug, Clone)]
pub struct RenderContext {
    /// Directory holding the Markdown file. Sidecar files are resolved against it.
    pub md_dir: PathBuf,
    /// Markdown file name without its extension. Used for auto-numbered file names.
    pub md_stem: String,
    /// Output longer than this many lines goes to a separate file.
    pub max_inline_lines: usize,
}

/// Content to be written to a separate file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// Absolute destination path (a path relative to `md_dir`, resolved).
    pub path: PathBuf,
    /// Link target placed in the Markdown (relative to `md_dir`).
    pub link: String,
    /// File content.
    pub contents: String,
}

/// The formatted result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultRender {
    /// Markdown of the result block. Spans the begin marker through the end
    /// marker and ends with a newline.
    pub markdown: String,
    /// Content to write out when a separate file is used.
    pub sidecar: Option<Sidecar>,
}

/// Builds a result block from a run outcome.
///
/// The caller passes `out_file_allowed` after actually resolving the destination
/// (checking that it stays under the Markdown directory, symlinks included).
/// Together with the lexical check performed here, that gives two layers of
/// validation.
pub fn render_result(
    cell: &Cell,
    outcome: &ExecOutcome,
    ctx: &RenderContext,
    out_file_allowed: bool,
) -> ResultRender {
    let output = normalize(&outcome.output);
    let line_count = count_lines(&output);

    // Markdown content is untrusted input, so a destination pointing outside the
    // Markdown directory is dropped entirely. Otherwise it could overwrite any file.
    let designated = cell
        .out_file
        .as_deref()
        .filter(|link| out_file_allowed && is_inside_dir(link));
    let rejected = cell.out_file.is_some() && designated.is_none();
    let summary = summary_line(outcome, line_count, rejected);

    let sidecar = sidecar_for(designated, ctx, cell, line_count, &output);
    let body = match &sidecar {
        // When the body already carries the link, do not repeat it.
        Some(_) if cell.out_file_in_text && !rejected => None,
        Some(sidecar) => Some(format!("[{}]({})", sidecar.link, sidecar.link)),
        None if output.is_empty() => Some("(no output)".to_string()),
        None => Some(fenced(&output)),
    };

    let markdown = match body {
        Some(body) => format!("{BEGIN_MARKER}\n{summary}\n\n{body}\n{END_MARKER}\n"),
        None => format!("{BEGIN_MARKER}\n{summary}\n{END_MARKER}\n"),
    };
    ResultRender { markdown, sidecar }
}

fn sidecar_for(
    designated: Option<&str>,
    ctx: &RenderContext,
    cell: &Cell,
    line_count: usize,
    output: &str,
) -> Option<Sidecar> {
    // An explicitly designated destination wins regardless of line count.
    let link = match designated {
        Some(path) => path.to_string(),
        None if line_count > ctx.max_inline_lines => {
            format!("{}-result-{}.txt", ctx.md_stem, cell.display_number())
        }
        None => return None,
    };
    Some(Sidecar {
        path: ctx.md_dir.join(&link),
        link,
        contents: if output.is_empty() {
            String::new()
        } else {
            format!("{output}\n")
        },
    })
}

/// Whether the destination stays under the directory holding the Markdown file.
fn is_inside_dir(link: &str) -> bool {
    let path = Path::new(link);
    path.is_relative()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn summary_line(outcome: &ExecOutcome, line_count: usize, rejected_out_file: bool) -> String {
    let timestamp = outcome.started_at.format("%Y-%m-%d %H:%M:%S");
    let seconds = outcome.duration.as_secs_f64();
    let mut summary = format!(
        "Ran result: {timestamp} ({}, {seconds:.2}s, {line_count} lines)",
        outcome.status_text()
    );
    if outcome.truncated {
        summary.push_str(" Output exceeded the limit and was truncated.");
    }
    if rejected_out_file {
        summary.push_str(
            " The designated output file points outside the Markdown directory and was ignored.",
        );
    }
    summary
}

/// Wraps the output in a code fence.
///
/// The fence is lengthened so that a run of backticks inside the output cannot
/// close it early.
fn fenced(output: &str) -> String {
    let longest = output
        .lines()
        // A fence can be closed even when indented, so strip leading spaces first.
        .map(|line| {
            line.trim_start_matches(' ')
                .chars()
                .take_while(|&c| c == '`')
                .count()
        })
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}text\n{output}\n{fence}")
}

/// Drops trailing newlines. Trailing spaces are part of the output and are left alone.
fn normalize(output: &str) -> String {
    output.trim_end_matches('\n').to_string()
}

fn count_lines(output: &str) -> usize {
    if output.is_empty() {
        return 0;
    }
    output.lines().count()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::TimeZone;

    use super::*;
    use crate::parse::Document;

    fn outcome(output: &str) -> ExecOutcome {
        ExecOutcome {
            started_at: chrono::Local
                .with_ymd_and_hms(2026, 8, 14, 9, 53, 32)
                .unwrap(),
            duration: Duration::from_millis(120),
            exit_code: Some(0),
            output: output.to_string(),
            timed_out: false,
            truncated: false,
        }
    }

    fn context(max_inline_lines: usize) -> RenderContext {
        RenderContext {
            md_dir: PathBuf::from("/tmp/notes"),
            md_stem: "exam".to_string(),
            max_inline_lines,
        }
    }

    fn cell(md: &str) -> Cell {
        Document::parse(md).cells.remove(0)
    }

    #[test]
    fn renders_inline_result() {
        let rendered = render_result(
            &cell("```shell\ndate\n```\n"),
            &outcome("Fri Aug 14 09:53:32 JST 2026\n"),
            &context(50),
            true,
        );
        assert_eq!(
            rendered.markdown,
            "<!-- runandlog:begin -->\nRan result: 2026-08-14 09:53:32 (exit 0, 0.12s, 1 lines)\n\n```text\nFri Aug 14 09:53:32 JST 2026\n```\n<!-- runandlog:end -->\n"
        );
        assert!(rendered.sidecar.is_none());
    }

    #[test]
    fn renders_empty_output() {
        let rendered = render_result(
            &cell("```shell\ntrue\n```\n"),
            &outcome(""),
            &context(50),
            true,
        );
        assert!(rendered.markdown.contains("(no output)"));
        assert!(rendered.sidecar.is_none());
    }

    #[test]
    fn lengthens_fence_when_output_contains_backticks() {
        let rendered = render_result(
            &cell("```shell\ndate\n```\n"),
            &outcome("```\ninner\n```\n"),
            &context(50),
            true,
        );
        assert!(
            rendered
                .markdown
                .contains("````text\n```\ninner\n```\n````")
        );
    }

    #[test]
    fn writes_long_output_to_a_sidecar_file() {
        let long = (1..=51)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render_result(
            &cell("```shell\nseq 51\n```\n"),
            &outcome(&long),
            &context(50),
            true,
        );
        let sidecar = rendered.sidecar.expect("sidecar");
        assert_eq!(sidecar.link, "exam-result-1.txt");
        assert_eq!(sidecar.path, PathBuf::from("/tmp/notes/exam-result-1.txt"));
        assert!(sidecar.contents.ends_with("51\n"));
        assert!(
            rendered
                .markdown
                .contains("[exam-result-1.txt](exam-result-1.txt)")
        );
        assert!(rendered.markdown.contains("51 lines"));
    }

    #[test]
    fn keeps_short_output_inline_at_the_boundary() {
        let output = (1..=50)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render_result(
            &cell("```shell\nseq 50\n```\n"),
            &outcome(&output),
            &context(50),
            true,
        );
        assert!(rendered.sidecar.is_none());
    }

    #[test]
    fn honours_designated_out_file_even_for_short_output() {
        let md =
            "```shell\ndate\n```\n\nResult:\n[date-command-result.txt](date-command-result.txt)\n";
        let rendered = render_result(&cell(md), &outcome("short\n"), &context(50), true);
        let sidecar = rendered.sidecar.expect("sidecar");
        assert_eq!(sidecar.link, "date-command-result.txt");
        assert_eq!(sidecar.contents, "short\n");
        // The Result: paragraph already carries the link, so the result block omits it.
        assert!(!rendered.markdown.contains("date-command-result.txt"));
    }

    #[test]
    fn repeats_the_link_when_the_out_file_comes_from_a_fence_attribute() {
        let md = "```shell out=uname.txt\nuname -a\n```\n";
        let rendered = render_result(&cell(md), &outcome("Linux\n"), &context(50), true);
        assert!(rendered.markdown.contains("[uname.txt](uname.txt)"));
    }

    #[test]
    fn ignores_an_out_file_outside_the_markdown_directory() {
        let md = "```shell out=../../etc/passwd\nid\n```\n";
        let rendered = render_result(&cell(md), &outcome("uid=0\n"), &context(50), true);
        assert!(rendered.sidecar.is_none());
        assert!(rendered.markdown.contains("was ignored"));
        assert!(rendered.markdown.contains("```text\nuid=0\n```"));
    }

    #[test]
    fn ignores_an_absolute_out_file() {
        let md = "```shell out=/etc/passwd\nid\n```\n";
        let rendered = render_result(&cell(md), &outcome("uid=0\n"), &context(50), true);
        assert!(rendered.sidecar.is_none());
    }

    #[test]
    fn ignores_an_out_file_that_the_caller_rejected() {
        let md = "```shell out=logs/id.txt\nid\n```\n";
        let rendered = render_result(&cell(md), &outcome("uid=0\n"), &context(50), false);
        assert!(rendered.sidecar.is_none());
        assert!(rendered.markdown.contains("was ignored"));
    }

    #[test]
    fn allows_an_out_file_in_a_subdirectory() {
        let md = "```shell out=logs/id.txt\nid\n```\n";
        let rendered = render_result(&cell(md), &outcome("uid=0\n"), &context(50), true);
        let sidecar = rendered.sidecar.expect("sidecar");
        assert_eq!(sidecar.path, PathBuf::from("/tmp/notes/logs/id.txt"));
    }

    #[test]
    fn lengthens_fence_for_indented_backticks_in_the_output() {
        let rendered = render_result(
            &cell("```shell\ndate\n```\n"),
            &outcome("   ```\n"),
            &context(50),
            true,
        );
        assert!(rendered.markdown.contains("````text"));
    }

    #[test]
    fn reports_truncation_in_the_summary() {
        let mut truncated = outcome("partial\n");
        truncated.truncated = true;
        let rendered = render_result(
            &cell("```shell\nyes\n```\n"),
            &truncated,
            &context(50),
            true,
        );
        assert!(rendered.markdown.contains("was truncated"));
    }

    #[test]
    fn reports_timeout_in_the_summary() {
        let mut timed_out = outcome("partial\n");
        timed_out.timed_out = true;
        let rendered = render_result(
            &cell("```shell\nsleep 30\n```\n"),
            &timed_out,
            &context(50),
            true,
        );
        assert!(rendered.markdown.contains("(timeout,"));
    }
}
