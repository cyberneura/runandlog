//! Extracts command cells and existing run results from Markdown.
//!
//! This is not a full Markdown parser; it only scans fenced code blocks.
//! To keep the original text intact when writing results back, the parse result
//! records positions as byte offsets into the source string, and edits are
//! applied as plain range replacements.

/// Opening marker of a result block.
///
/// Results are wrapped in HTML comments so that a re-run can reliably locate and
/// replace the previous result. HTML comments are valid Markdown and are not
/// displayed when rendered.
pub const BEGIN_MARKER: &str = "<!-- runandlog:begin -->";
/// Closing marker of a result block.
pub const END_MARKER: &str = "<!-- runandlog:end -->";

const BEGIN_MARKER_PREFIX: &str = "<!-- runandlog:begin";

/// Code fence languages treated as runnable cells.
const SHELL_LANGS: [&str; 4] = ["shell", "sh", "bash", "zsh"];

/// A single command cell in a Markdown document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Zero-based cell index (order of appearance in the document).
    pub index: usize,
    /// Fence language (`shell` and friends).
    pub lang: String,
    /// Fence body. Multiple lines are run together in a single shell invocation.
    pub command: String,
    /// File explicitly designated as the destination for the result.
    /// Relative to the Markdown file.
    pub out_file: Option<String>,
    /// Whether the destination is written in the body as a `Result:` paragraph.
    ///
    /// When the body already carries the link, the result block does not repeat it.
    pub out_file_in_text: bool,
    /// Byte offset of the start of the opening fence line.
    pub fence_start: usize,
    /// Byte offset just past the closing fence line.
    pub fence_end: usize,
    /// Range of an existing result block
    /// (start of the begin-marker line .. just past the end-marker line).
    pub result_span: Option<(usize, usize)>,
    /// Position to insert at when there is no result block yet.
    pub insert_at: usize,
}

impl Cell {
    /// One-based cell number, for display.
    pub fn display_number(&self) -> usize {
        self.index + 1
    }
}

/// A parsed Markdown document.
#[derive(Debug, Clone)]
pub struct Document {
    /// The original text.
    pub text: String,
    /// The command cells that were found.
    pub cells: Vec<Cell>,
}

/// An edit that replaces a slice of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

impl Document {
    /// Parses Markdown.
    pub fn parse(text: &str) -> Document {
        let lines = split_lines(text);
        let mut cells = Vec::new();
        let mut i = 0;

        while i < lines.len() {
            let Some(fence) = open_fence(lines[i].text) else {
                i += 1;
                continue;
            };
            let close = (i + 1..lines.len()).find(|&j| closes_fence(lines[j].text, &fence));
            let block_end = match close {
                Some(j) => lines[j].end,
                None => text.len(),
            };
            let next = close.map(|j| j + 1).unwrap_or(lines.len());

            if let Some((lang, attrs)) = shell_fence_info(&fence.info) {
                let body_end = close.map(|j| lines[j].start).unwrap_or(text.len());
                let command = text[lines[i].end.min(body_end)..body_end].to_string();
                let trailer = scan_trailer(text, &lines, next);
                let out_file_in_text = attrs.out.is_none() && trailer.designated_file.is_some();
                cells.push(Cell {
                    index: cells.len(),
                    lang,
                    command: dedent(&command, fence.indent),
                    out_file: attrs.out.or(trailer.designated_file),
                    out_file_in_text,
                    fence_start: lines[i].start,
                    fence_end: block_end,
                    result_span: trailer.result_span,
                    insert_at: trailer.insert_at.unwrap_or(block_end),
                });
            }
            i = next;
        }

        Document {
            text: text.to_string(),
            cells,
        }
    }

    /// Returns the body of an existing result block (without the marker lines).
    pub fn result_text(&self, cell: &Cell) -> Option<&str> {
        let (start, end) = cell.result_span?;
        let block = &self.text[start..end];
        let body_start = block.find('\n').map(|i| i + 1).unwrap_or(block.len());
        let body_end = block.rfind(END_MARKER).unwrap_or(block.len());
        Some(block[body_start..body_end.max(body_start)].trim_matches('\n'))
    }

    /// Builds an edit that replaces the result block of `cell` with `region`,
    /// inserting one if it does not exist yet.
    ///
    /// `region` must include the end-marker line and must end with a newline.
    pub fn result_edit(&self, cell: &Cell, region: &str) -> Edit {
        match cell.result_span {
            Some((start, end)) => Edit {
                start,
                end,
                replacement: region.to_string(),
            },
            None => {
                let at = cell.insert_at;
                let rest = &self.text[at..];
                let mut replacement = String::new();
                // Always keep exactly one blank line after the preceding block.
                if !self.text[..at].ends_with("\n\n") {
                    replacement.push('\n');
                }
                replacement.push_str(region);
                // Keep the result block from running into the text that follows.
                if !rest.is_empty() && !rest.starts_with('\n') {
                    replacement.push('\n');
                }
                Edit {
                    start: at,
                    end: at,
                    replacement,
                }
            }
        }
    }
}

/// Applies several edits at once. The caller is responsible for making sure the
/// ranges do not overlap.
pub fn splice(text: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| e.start);
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for edit in edits {
        if edit.start < pos {
            continue;
        }
        out.push_str(&text[pos..edit.start]);
        out.push_str(&edit.replacement);
        pos = edit.end;
    }
    out.push_str(&text[pos..]);
    out
}

struct Line<'a> {
    text: &'a str,
    /// Byte offset of the start of the line.
    start: usize,
    /// Byte offset of the end of the line, including the newline.
    end: usize,
}

fn split_lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = match text[start..].find('\n') {
            Some(offset) => start + offset + 1,
            None => text.len(),
        };
        lines.push(Line {
            text: text[start..end]
                .trim_end_matches('\n')
                .trim_end_matches('\r'),
            start,
            end,
        });
        start = end;
    }
    lines
}

struct Fence {
    ch: char,
    len: usize,
    indent: usize,
    info: String,
}

fn open_fence(line: &str) -> Option<Fence> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = rest.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    let info = rest[len..].trim().to_string();
    // A backtick fence cannot carry backticks in its info string.
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some(Fence {
        ch,
        len,
        indent,
        info,
    })
}

fn closes_fence(line: &str, fence: &Fence) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    let len = trimmed.chars().take_while(|&c| c == fence.ch).count();
    len >= fence.len && trimmed[len..].trim().is_empty()
}

#[derive(Default)]
struct FenceAttrs {
    out: Option<String>,
}

/// Returns (language, attributes) if the fence info string denotes a shell cell.
///
/// Attributes follow the language as `out=path`
/// (for example ```` ```shell out=result.txt ````).
fn shell_fence_info(info: &str) -> Option<(String, FenceAttrs)> {
    let mut tokens = tokenize_info(info);
    if tokens.is_empty() {
        return None;
    }
    let lang = tokens.remove(0);
    if !SHELL_LANGS.contains(&lang.to_ascii_lowercase().as_str()) {
        return None;
    }
    let mut attrs = FenceAttrs::default();
    for token in tokens {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        if matches!(key, "out" | "file" | "result") {
            attrs.out = Some(value.trim_matches('"').to_string());
        }
    }
    Some((lang, attrs))
}

/// Splits an info string on whitespace. Values may be wrapped in double quotes.
fn tokenize_info(info: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in info.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[derive(Default)]
struct Trailer {
    designated_file: Option<String>,
    result_span: Option<(usize, usize)>,
    insert_at: Option<usize>,
}

/// Reads what follows the closing fence, picking up a designated output file and
/// any existing result block.
fn scan_trailer(text: &str, lines: &[Line<'_>], from: usize) -> Trailer {
    let mut trailer = Trailer::default();
    let mut i = skip_blank(lines, from);

    // Output file designated up front by a "Result:" paragraph.
    if i < lines.len() && is_designation_head(lines[i].text) {
        let end = (i..lines.len())
            .find(|&j| lines[j].text.trim().is_empty())
            .unwrap_or(lines.len());
        let paragraph = &text[lines[i].start..lines[end - 1].end];
        trailer.designated_file = extract_link_target(paragraph);
        trailer.insert_at = Some(lines[end - 1].end);
        i = skip_blank(lines, end);
    }

    // An existing result block.
    if i < lines.len()
        && lines[i].text.trim_start().starts_with(BEGIN_MARKER_PREFIX)
        && let Some(end) = find_end_marker(lines, i + 1)
    {
        trailer.result_span = Some((lines[i].start, lines[end].end));
    }
    trailer
}

/// Finds the end-marker line of a result block.
///
/// Command output can itself contain a line identical to the end marker, so the
/// scan skips over code fences. Output is always written inside a fence, which
/// is what makes this reliable.
fn find_end_marker(lines: &[Line<'_>], from: usize) -> Option<usize> {
    let mut i = from;
    while i < lines.len() {
        if let Some(fence) = open_fence(lines[i].text) {
            i = (i + 1..lines.len())
                .find(|&j| closes_fence(lines[j].text, &fence))
                .map(|j| j + 1)
                .unwrap_or(lines.len());
            continue;
        }
        if lines[i].text.trim() == END_MARKER {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn skip_blank(lines: &[Line<'_>], from: usize) -> usize {
    let mut i = from;
    while i < lines.len() && lines[i].text.trim().is_empty() {
        i += 1;
    }
    i
}

/// `結果:` is accepted alongside `Result:` because these documents are often
/// written in Japanese; it is an accepted input spelling, not UI text.
fn is_designation_head(line: &str) -> bool {
    let trimmed = line.trim_start().trim_start_matches(['*', '_', ' ']);
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("result:") || trimmed.starts_with("結果:")
}

/// Extracts the first link target from a paragraph.
///
/// `[label](path)` is the normal form; a bare `[path]` without a target is also
/// accepted.
fn extract_link_target(paragraph: &str) -> Option<String> {
    let open = paragraph.find('[')?;
    let close = paragraph[open..].find(']')? + open;
    let label = paragraph[open + 1..close].trim();
    let rest = paragraph[close + 1..].trim_start();
    if let Some(stripped) = rest.strip_prefix('(') {
        let end = closing_paren(stripped)?;
        let target = stripped[..end].trim();
        if !target.is_empty() {
            return Some(undelimit(target));
        }
    }
    if label.is_empty() {
        return None;
    }
    Some(undelimit(label))
}

/// Finds the byte offset of the `)` that closes an inline link destination.
///
/// Parentheses inside a destination are allowed as long as they balance, so a file
/// called `run(1).txt` must not be cut at its first `)` -- doing so would write the
/// output to `run(1` while the rendered link still pointed at `run(1).txt`.
fn closing_paren(rest: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (offset, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '(' => depth += 1,
            ')' if depth == 0 => return Some(offset),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Unwraps the `<...>` form of a link destination.
///
/// A destination containing spaces has to be written that way -- `render` emits
/// exactly this form -- so the brackets are delimiters, not part of the file name.
/// Reading them literally would create a file actually called `<a b.txt>` while
/// the rendered link pointed at `a b.txt`, i.e. output that looks lost.
fn undelimit(target: &str) -> String {
    let Some(inner) = target.strip_prefix('<').and_then(|t| t.strip_suffix('>')) else {
        return target.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                // Only the characters render escapes are unescaped; anything else
                // keeps its backslash, since a file name may legitimately contain one.
                Some(escaped @ ('<' | '>' | '\\')) => out.push(escaped),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            _ => out.push(ch),
        }
    }
    out
}

/// Removes as much leading indentation from a fence body as the fence itself has.
fn dedent(body: &str, indent: usize) -> String {
    if indent == 0 {
        return body.to_string();
    }
    body.lines()
        .map(|line| {
            let strip = line.len() - line.trim_start_matches(' ').len();
            &line[strip.min(indent)..]
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_shell_cells_and_ignores_other_languages() {
        let md = "# title\n\n```shell\ndate\n```\n\n```python\nprint(1)\n```\n\n```sh\nls /opt\nls /tmp\n```\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells.len(), 2);
        assert_eq!(doc.cells[0].command, "date\n");
        assert_eq!(doc.cells[1].command, "ls /opt\nls /tmp\n");
        assert_eq!(doc.cells[1].display_number(), 2);
    }

    #[test]
    fn ignores_fences_without_language() {
        let doc = Document::parse("```\ndate\n```\n");
        assert!(doc.cells.is_empty());
    }

    #[test]
    fn inserts_result_after_fence() {
        let md = "```shell\ndate\n```\n\nnext paragraph\n";
        let doc = Document::parse(md);
        let region = format!("{BEGIN_MARKER}\nRan\n{END_MARKER}\n");
        let edit = doc.result_edit(&doc.cells[0], &region);
        let out = splice(md, vec![edit]);
        assert_eq!(
            out,
            "```shell\ndate\n```\n\n<!-- runandlog:begin -->\nRan\n<!-- runandlog:end -->\n\nnext paragraph\n"
        );
    }

    #[test]
    fn inserts_result_when_fence_is_last_line_without_trailing_newline() {
        let md = "```shell\ndate\n```";
        let doc = Document::parse(md);
        let region = format!("{BEGIN_MARKER}\nRan\n{END_MARKER}\n");
        let out = splice(md, vec![doc.result_edit(&doc.cells[0], &region)]);
        assert_eq!(
            out,
            "```shell\ndate\n```\n<!-- runandlog:begin -->\nRan\n<!-- runandlog:end -->\n"
        );
    }

    #[test]
    fn replaces_existing_result_block() {
        let md = "```shell\ndate\n```\n\n<!-- runandlog:begin -->\nold\n<!-- runandlog:end -->\n\ntail\n";
        let doc = Document::parse(md);
        assert!(doc.cells[0].result_span.is_some());
        let region = format!("{BEGIN_MARKER}\nnew\n{END_MARKER}\n");
        let out = splice(md, vec![doc.result_edit(&doc.cells[0], &region)]);
        assert_eq!(
            out,
            "```shell\ndate\n```\n\n<!-- runandlog:begin -->\nnew\n<!-- runandlog:end -->\n\ntail\n"
        );
    }

    #[test]
    fn result_block_of_previous_cell_is_not_stolen_by_next_cell() {
        let md = "```shell\na\n```\n\n<!-- runandlog:begin -->\nold\n<!-- runandlog:end -->\n\n```shell\nb\n```\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells.len(), 2);
        assert!(doc.cells[0].result_span.is_some());
        assert!(doc.cells[1].result_span.is_none());
    }

    #[test]
    fn reads_out_file_from_fence_attribute() {
        let doc = Document::parse("```shell out=\"date result.txt\"\ndate\n```\n");
        assert_eq!(doc.cells[0].out_file.as_deref(), Some("date result.txt"));
    }

    #[test]
    fn reads_out_file_from_result_paragraph() {
        let md = "```shell\ndate\n```\n\nResult:\n[date-command-result.txt](date-command-result.txt)\n\ntail\n";
        let doc = Document::parse(md);
        assert_eq!(
            doc.cells[0].out_file.as_deref(),
            Some("date-command-result.txt")
        );
        let region = format!("{BEGIN_MARKER}\nRan\n{END_MARKER}\n");
        let out = splice(md, vec![doc.result_edit(&doc.cells[0], &region)]);
        assert!(out.contains(
            "[date-command-result.txt](date-command-result.txt)\n\n<!-- runandlog:begin -->"
        ));
    }

    #[test]
    fn reads_an_angle_bracketed_out_file() {
        // render emits this form for destinations containing spaces, so parse has to
        // read it back or the file name would gain literal angle brackets.
        let md = "```shell\ndate\n```\n\nResult:\n[date result.txt](<date result.txt>)\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells[0].out_file.as_deref(), Some("date result.txt"));
    }

    #[test]
    fn unescapes_an_angle_bracketed_out_file() {
        let md = "```shell\ndate\n```\n\nResult:\n[x](<a \\<b\\> c.txt>)\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells[0].out_file.as_deref(), Some("a <b> c.txt"));
    }

    #[test]
    fn keeps_angle_brackets_that_are_not_delimiters() {
        let md = "```shell\ndate\n```\n\nResult:\n[x](a<b.txt)\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells[0].out_file.as_deref(), Some("a<b.txt"));
    }

    #[test]
    fn reads_an_out_file_containing_balanced_parentheses() {
        let md = "```shell\ndate\n```\n\nResult:\n[result](reports/run(1).txt)\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells[0].out_file.as_deref(), Some("reports/run(1).txt"));
    }

    #[test]
    fn stops_at_the_closing_paren_of_the_link() {
        let md = "```shell\ndate\n```\n\nResult:\n[result](out.txt) and (more text)\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells[0].out_file.as_deref(), Some("out.txt"));
    }

    #[test]
    fn accepts_bare_bracket_designation() {
        let md = "```shell\ndate\n```\n\nResult:\n[out.txt]\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells[0].out_file.as_deref(), Some("out.txt"));
    }

    #[test]
    fn end_marker_inside_the_output_does_not_close_the_result_block() {
        let md = "```shell\ncat log\n```\n\n<!-- runandlog:begin -->\nRan\n\n````text\n<!-- runandlog:end -->\nstill output\n````\n<!-- runandlog:end -->\n\ntail\n";
        let doc = Document::parse(md);
        let region = format!("{BEGIN_MARKER}\nnew\n{END_MARKER}\n");
        let out = splice(md, vec![doc.result_edit(&doc.cells[0], &region)]);
        assert_eq!(
            out,
            "```shell\ncat log\n```\n\n<!-- runandlog:begin -->\nnew\n<!-- runandlog:end -->\n\ntail\n"
        );
    }

    #[test]
    fn unclosed_result_block_is_ignored() {
        let md = "```shell\ndate\n```\n\n<!-- runandlog:begin -->\nbroken\n";
        let doc = Document::parse(md);
        assert!(doc.cells[0].result_span.is_none());
    }

    #[test]
    fn handles_longer_fences_and_nested_backticks() {
        let md = "````shell\necho '```'\n````\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells.len(), 1);
        assert_eq!(doc.cells[0].command, "echo '```'\n");
    }

    #[test]
    fn dedents_indented_fence_body() {
        let md = "  ```shell\n  date\n  ```\n";
        let doc = Document::parse(md);
        assert_eq!(doc.cells[0].command, "date");
    }

    #[test]
    fn splices_multiple_cells_at_once() {
        let md = "```shell\na\n```\n\n```shell\nb\n```\n";
        let doc = Document::parse(md);
        let edits = doc
            .cells
            .iter()
            .map(|cell| {
                doc.result_edit(
                    cell,
                    &format!("{BEGIN_MARKER}\nr{}\n{END_MARKER}\n", cell.display_number()),
                )
            })
            .collect();
        let out = splice(md, edits);
        assert!(out.contains("\nr1\n"));
        assert!(out.contains("\nr2\n"));
    }
}
