//! Markdown からコマンドセルと既存の実行結果を取り出す。
//!
//! Markdown の完全なパーサーではなく、フェンスドコードブロックの走査に絞っている。
//! 書き戻し時に原文をできるだけそのまま保つため、パース結果は文字列のバイトオフセットで
//! 位置を保持し、編集は該当範囲の置換だけで行う。

/// 実行結果ブロックの開始マーカー。
///
/// 再実行時に前回の結果を確実に置き換えられるよう、結果は HTML コメントで挟む。
/// HTML コメントは Markdown として妥当で、レンダリング時には表示されない。
pub const BEGIN_MARKER: &str = "<!-- runandlog:begin -->";
/// 実行結果ブロックの終了マーカー。
pub const END_MARKER: &str = "<!-- runandlog:end -->";

const BEGIN_MARKER_PREFIX: &str = "<!-- runandlog:begin";

/// 実行対象とみなすコードフェンスの言語。
const SHELL_LANGS: [&str; 4] = ["shell", "sh", "bash", "zsh"];

/// Markdown 中の 1 つのコマンドセル。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// 0 始まりのセル番号 (ドキュメント内の出現順)。
    pub index: usize,
    /// フェンスの言語 (`shell` 等)。
    pub lang: String,
    /// フェンスの中身。複数行ある場合はまとめて 1 回のシェル起動で実行する。
    pub command: String,
    /// 結果の書き出し先として明示指定されたファイル。Markdown ファイルからの相対パス。
    pub out_file: Option<String>,
    /// 書き出し先が `Result:` 段落として本文に書かれているか。
    ///
    /// 本文に既にリンクがあるなら、結果ブロックで同じリンクを繰り返さない。
    pub out_file_in_text: bool,
    /// 開きフェンス行の先頭バイトオフセット。
    pub fence_start: usize,
    /// 閉じフェンス行の直後のバイトオフセット。
    pub fence_end: usize,
    /// 既存の結果ブロックの範囲 (開始マーカー行の先頭 .. 終了マーカー行の直後)。
    pub result_span: Option<(usize, usize)>,
    /// 結果ブロックが無い場合に挿入する位置。
    pub insert_at: usize,
}

impl Cell {
    /// 1 始まりの表示用セル番号。
    pub fn display_number(&self) -> usize {
        self.index + 1
    }
}

/// パース済みの Markdown ドキュメント。
#[derive(Debug, Clone)]
pub struct Document {
    /// 原文。
    pub text: String,
    /// 見つかったコマンドセル。
    pub cells: Vec<Cell>,
}

/// 文字列の一部を置き換える編集。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

impl Document {
    /// Markdown をパースする。
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

    /// 既存の結果ブロックの中身 (マーカー行を除いた部分) を返す。
    pub fn result_text(&self, cell: &Cell) -> Option<&str> {
        let (start, end) = cell.result_span?;
        let block = &self.text[start..end];
        let body_start = block.find('\n').map(|i| i + 1).unwrap_or(block.len());
        let body_end = block.rfind(END_MARKER).unwrap_or(block.len());
        Some(block[body_start..body_end.max(body_start)].trim_matches('\n'))
    }

    /// 指定したセルの結果ブロックを `region` で置き換える (無ければ挿入する) 編集を作る。
    ///
    /// `region` は終了マーカー行までを含み、末尾は改行で終わっていること。
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
                // 直前のブロックとの間に必ず空行を 1 つ入れる。
                if !self.text[..at].ends_with("\n\n") {
                    replacement.push('\n');
                }
                replacement.push_str(region);
                // 後続の本文と結果ブロックがくっつかないようにする。
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

/// 複数の編集をまとめて適用する。範囲が重ならないことは呼び出し側の責任。
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
    /// 行頭のバイトオフセット。
    start: usize,
    /// 改行を含む行末のバイトオフセット。
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
    // バッククォートのフェンスでは info にバッククォートを含められない。
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

/// フェンスの info 文字列がシェルセルなら (言語, 属性) を返す。
///
/// 属性は言語に続けて `out=path` の形で書ける (例: ```` ```shell out=result.txt ````)。
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

/// info 文字列を空白区切りで分割する。値はダブルクォートで囲める。
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

/// 閉じフェンスの後ろを読み、結果の書き出し先指定と既存の結果ブロックを拾う。
fn scan_trailer(text: &str, lines: &[Line<'_>], from: usize) -> Trailer {
    let mut trailer = Trailer::default();
    let mut i = skip_blank(lines, from);

    // "Result:" 段落による書き出し先の事前指定。
    if i < lines.len() && is_designation_head(lines[i].text) {
        let end = (i..lines.len())
            .find(|&j| lines[j].text.trim().is_empty())
            .unwrap_or(lines.len());
        let paragraph = &text[lines[i].start..lines[end - 1].end];
        trailer.designated_file = extract_link_target(paragraph);
        trailer.insert_at = Some(lines[end - 1].end);
        i = skip_blank(lines, end);
    }

    // 既存の結果ブロック。
    if i < lines.len()
        && lines[i].text.trim_start().starts_with(BEGIN_MARKER_PREFIX)
        && let Some(end) = find_end_marker(lines, i + 1)
    {
        trailer.result_span = Some((lines[i].start, lines[end].end));
    }
    trailer
}

/// 結果ブロックの終了マーカー行を探す。
///
/// コマンドの出力自体が終了マーカーと同じ行を含むことがあるため、コードフェンスの中は読み飛ばす。
/// 出力は必ずフェンスで囲んで書き込むので、これで取り違えを防げる。
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

fn is_designation_head(line: &str) -> bool {
    let trimmed = line.trim_start().trim_start_matches(['*', '_', ' ']);
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("result:") || trimmed.starts_with("結果:")
}

/// 段落から最初のリンク先を取り出す。
///
/// `[名前](パス)` を基本とし、リンク先が省略された `[パス]` も許容する。
fn extract_link_target(paragraph: &str) -> Option<String> {
    let open = paragraph.find('[')?;
    let close = paragraph[open..].find(']')? + open;
    let label = paragraph[open + 1..close].trim();
    let rest = paragraph[close + 1..].trim_start();
    if let Some(stripped) = rest.strip_prefix('(') {
        let end = stripped.find(')')?;
        let target = stripped[..end].trim();
        if !target.is_empty() {
            return Some(target.to_string());
        }
    }
    if label.is_empty() {
        return None;
    }
    Some(label.to_string())
}

/// インデントされたフェンスの中身から、フェンスと同じだけの字下げを取り除く。
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
