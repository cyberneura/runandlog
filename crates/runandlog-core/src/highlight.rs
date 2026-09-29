//! Syntax highlighting of a cell's command.
//!
//! A small shell lexer, not a parser: it only has to colour what a reader skims
//! for -- which word is the program, which are options, where a string or a
//! comment runs -- and it must never fail, because the text it is given is
//! whatever the Markdown holds, finished or not. Anything it does not recognise
//! is left plain.
//!
//! The front ends share this so that the TUI and the GUI colour a command the
//! same way; neither knows anything about shell syntax itself.

/// What a piece of a command is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Nothing to colour: arguments, whitespace, anything unrecognised.
    Plain,
    /// The word run as a program (the first word of a simple command).
    Command,
    /// A reserved word (`if`, `for`, `do`, `done`, ...).
    Keyword,
    /// An argument starting with `-`.
    Option,
    /// A quoted string, or the body of a here-document.
    String,
    /// A parameter expansion (`$HOME`, `${x}`), `$(` / `` ` `` opening a command
    /// substitution, or the name in an assignment.
    Variable,
    /// From an unquoted `#` at the start of a word to the end of the line.
    Comment,
    /// Control and redirection operators (`|`, `&&`, `;`, `>`, `2>`, `<<`, ...).
    Operator,
}

impl TokenKind {
    /// Stable lower-case name, for front ends that style by name (the GUI's CSS).
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Plain => "plain",
            TokenKind::Command => "command",
            TokenKind::Keyword => "keyword",
            TokenKind::Option => "option",
            TokenKind::String => "string",
            TokenKind::Variable => "variable",
            TokenKind::Comment => "comment",
            TokenKind::Operator => "operator",
        }
    }
}

/// A run of the command text of one kind, as a byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

/// Reserved words after which a command is expected.
const KEYWORDS_BEFORE_COMMAND: &[&str] = &[
    "if", "then", "else", "elif", "while", "until", "do", "!", "{", "time",
];
/// Reserved words that end a construct; what follows is an operator or nothing.
const KEYWORDS_CLOSING: &[&str] = &["fi", "done", "esac", "}"];
/// Reserved words followed by a name rather than a command.
const KEYWORDS_BEFORE_NAME: &[&str] = &["for", "case", "select", "function"];

/// Splits `src` into tokens that cover it end to end, in order, with adjacent
/// tokens of the same kind merged.
pub fn highlight(src: &str) -> Vec<Token> {
    let mut lexer = Lexer {
        src,
        pos: 0,
        tokens: Vec::new(),
        command_expected: true,
        after_for_or_case: Stage::None,
        heredocs: Vec::new(),
        redirect_target: false,
        nesting: Vec::new(),
        case_pending: false,
        function_pending: false,
        word_continues: false,
        cases: Vec::new(),
        after_time: false,
    };
    lexer.run();
    lexer.tokens
}

/// Where the lexer is in `for NAME in ...` / `case WORD in ...`, so that `in`
/// is coloured there and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    None,
    /// Just read `for` / `case` / `select`: the name comes next.
    Name,
    /// Read the name: `in` may come next.
    In,
}

/// A here-document whose body starts at the next newline.
struct Heredoc {
    delimiter: String,
    /// `<<-`: leading tabs are stripped from the closing line.
    strip_tabs: bool,
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    tokens: Vec<Token>,
    /// Whether the next word is in command position.
    command_expected: bool,
    after_for_or_case: Stage,
    /// Here-documents opened on the current line, in order.
    heredocs: Vec<Heredoc>,
    /// The next word is the target of a redirection: neither a command nor the
    /// end of command position (`> out.txt echo hi` still runs `echo`).
    redirect_target: bool,
    /// Open command substitutions and subshells, innermost last, each with the
    /// `command_expected` to go back to once it closes (`A=$(date) env` runs `env`).
    nesting: Vec<Nest>,
    /// Read `case`, and its `in` is still to come.
    case_pending: bool,
    /// Read `function`: the next word is the name being defined.
    function_pending: bool,
    /// A substitution just closed in the middle of a word (`PATH=$(pwd)/bin`):
    /// what follows is the rest of that word, not a new one.
    word_continues: bool,
    /// Open `case` statements, innermost last.
    cases: Vec<Case>,
    /// Read `time`: options (`-p`) may come before the command.
    after_time: bool,
}

/// An open `case` statement.
#[derive(Debug, Clone, Copy)]
struct Case {
    /// Reading an arm's pattern (after `in` or `;;`) rather than its command list
    /// (after `)`).
    pattern: bool,
    /// `nesting` depth the statement sits at. A substitution inside a pattern
    /// (`$(printf x))`) is deeper, and its `)` is not the one that ends the pattern.
    depth: usize,
}

/// Something opened that a later `)` or backquote closes.
#[derive(Debug, Clone, Copy)]
struct Nest {
    closer: Closer,
    command_expected_after: bool,
    /// An expression rather than commands -- arithmetic (`$((`, `((`), a
    /// conditional (`[[`) or an array value (`=(`): nothing inside is a command,
    /// not even after `(`, `&&`, `;` or a newline.
    expression: bool,
    /// The `for` / `case` progress outside, which the inside must not disturb
    /// (`case $(printf y) in` still reaches its `in`).
    stage: Stage,
    case_pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closer {
    Paren,
    Backquote,
    /// `]]`, closing `[[`.
    Brackets,
}

impl<'a> Lexer<'a> {
    fn run(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                '\n' => {
                    self.push(TokenKind::Plain, 1);
                    // A newline inside an expression is only whitespace.
                    self.command_expected = !self.in_expression();
                    self.redirect_target = false;
                    self.read_heredoc_bodies();
                }
                ' ' | '\t' | '\r' => self.push(TokenKind::Plain, c.len_utf8()),
                '\\' if self.rest().starts_with("\\\n") => {
                    // A line continuation: the command goes on, so the state stays.
                    self.push(TokenKind::Plain, 2);
                }
                '#' if !self.word_continues => {
                    let len = self.rest().find('\n').unwrap_or(self.rest().len());
                    self.push(TokenKind::Comment, len);
                }
                _ => {
                    if let Some(len) = operator_len(self.rest()) {
                        self.operator(len);
                    } else {
                        self.word();
                    }
                }
            }
        }
    }

    fn rest(&self) -> &'a str {
        let src: &'a str = self.src;
        &src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Records the next `len` bytes as `kind`.
    fn push(&mut self, kind: TokenKind, len: usize) {
        let (start, end) = (self.pos, (self.pos + len).min(self.src.len()));
        self.pos = end;
        if start == end {
            return;
        }
        match self.tokens.last_mut() {
            Some(last) if last.kind == kind && last.end == start => last.end = end,
            _ => self.tokens.push(Token { kind, start, end }),
        }
    }

    fn operator(&mut self, len: usize) {
        let op = &self.rest()[..len];
        let array_value = self.src[..self.pos].ends_with('=');
        self.push(TokenKind::Operator, len);
        self.redirect_target = false;
        if self.in_case_pattern() && !matches!(op, "<(" | ">(") {
            // `a|b)` and the optional `(` of `(a)`: all part of the pattern.
            // A process substitution opens as anywhere else, below.
            if op == ")" {
                self.set_case_pattern(false);
                self.command_expected = true;
            }
            return;
        }
        if matches!(op, ";;" | ";&" | ";;&") && !self.cases.is_empty() {
            self.set_case_pattern(true);
            return;
        }
        match op {
            "<<" | "<<-" => self.heredoc_delimiter(op == "<<-"),
            // Process substitution: a command inside, an argument outside.
            "<(" | ">(" => self.open(Closer::Paren, self.command_expected, false),
            // Grouping inside an expression stays an expression.
            "(" if self.in_expression() => self.open(Closer::Paren, false, true),
            // `name=(one two)`: an array's elements, not a subshell.
            "(" if array_value => self.open(Closer::Paren, self.command_expected, true),
            // `((`: an arithmetic command, closed by `))`.
            "(" if self.rest().starts_with('(') => {
                self.push(TokenKind::Operator, 1);
                self.open(Closer::Paren, false, true);
                self.open(Closer::Paren, false, true);
            }
            // A subshell: whatever follows its `)` is an operator, not a command.
            "(" => self.open(Closer::Paren, false, false),
            ")" => self.close(Closer::Paren),
            _ if op.contains(['<', '>']) => self.redirect_target = true,
            // `;` separates the clauses of `for ((i = 0; i < n; i++))`, and `&&`
            // joins the tests of `[[ -f a && -f b ]]`.
            _ if self.in_expression() => {}
            _ => {
                // `|`, `&&`, `;` and the rest start a new command.
                self.command_expected = true;
                self.after_for_or_case = Stage::None;
            }
        }
    }

    fn in_expression(&self) -> bool {
        self.nesting.last().is_some_and(|n| n.expression)
    }

    fn in_case_pattern(&self) -> bool {
        self.cases
            .last()
            .is_some_and(|case| case.pattern && case.depth == self.nesting.len())
    }

    fn set_case_pattern(&mut self, pattern: bool) {
        if let Some(top) = self.cases.last_mut() {
            top.pattern = pattern;
        }
    }

    /// Enters a substitution, subshell or expression. `after` is `command_expected`
    /// once it closes; inside, a command is expected unless it is an `expression`.
    fn open(&mut self, closer: Closer, after: bool, expression: bool) {
        self.nesting.push(Nest {
            closer,
            command_expected_after: after,
            expression,
            stage: std::mem::replace(&mut self.after_for_or_case, Stage::None),
            case_pending: std::mem::take(&mut self.case_pending),
        });
        self.command_expected = !expression;
    }

    /// Leaves the innermost substitution or subshell, if it is closed by `closer`.
    /// A stray closer (a `case` pattern's `)`, say) just ends command position.
    fn close(&mut self, closer: Closer) {
        match self.nesting.last() {
            Some(&nest) if nest.closer == closer => {
                self.command_expected = nest.command_expected_after;
                self.after_for_or_case = nest.stage;
                self.case_pending = nest.case_pending;
                self.nesting.pop();
                self.word_continues = self
                    .peek()
                    .is_some_and(|c| !c.is_whitespace() && operator_len(self.rest()).is_none());
            }
            _ => self.command_expected = false,
        }
    }

    /// Reads the delimiter after `<<`, remembering it for the body.
    fn heredoc_delimiter(&mut self, strip_tabs: bool) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.push(TokenKind::Plain, 1);
        }
        let len = word_len(self.rest());
        if len == 0 {
            return;
        }
        let delimiter: String = self.rest()[..len]
            .chars()
            .filter(|c| !matches!(c, '\'' | '"' | '\\'))
            .collect();
        self.push(TokenKind::String, len);
        self.heredocs.push(Heredoc {
            delimiter,
            strip_tabs,
        });
    }

    /// Consumes the bodies of the here-documents opened on the line just ended.
    fn read_heredoc_bodies(&mut self) {
        for heredoc in std::mem::take(&mut self.heredocs) {
            loop {
                if self.pos >= self.src.len() {
                    return;
                }
                let line_len = self.rest().find('\n').map_or(self.rest().len(), |i| i + 1);
                let line = self.rest()[..line_len].trim_end_matches(['\n', '\r']);
                let line = if heredoc.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line
                };
                let closing = line == heredoc.delimiter;
                self.push(
                    if closing {
                        TokenKind::Operator
                    } else {
                        TokenKind::String
                    },
                    line_len,
                );
                if closing {
                    break;
                }
            }
        }
    }

    /// A word, which may be a mix of plain text, quotes and expansions.
    fn word(&mut self) {
        let start = self.pos;
        let len = word_len(self.rest());
        let src: &'a str = self.src;
        let text = &src[start..start + len];

        // A file descriptor number directly before a redirection (`2>`) belongs to it.
        if !text.is_empty()
            && text.bytes().all(|b| b.is_ascii_digit())
            && self.src[start + len..].starts_with(['<', '>'])
        {
            self.push(TokenKind::Operator, len);
            return;
        }

        if text == "]]"
            && self
                .nesting
                .last()
                .is_some_and(|n| n.closer == Closer::Brackets)
        {
            self.push(TokenKind::Keyword, len);
            self.close(Closer::Brackets);
            return;
        }

        if std::mem::take(&mut self.word_continues) {
            // The rest of a word a substitution was in the middle of. It carries on
            // as whatever that word was, so command position stays as it is.
            self.word_body(start + len, TokenKind::Plain);
            self.after_opener(text);
            return;
        }

        if self.redirect_target {
            // The file a redirection reads or writes. Command position is untouched.
            self.redirect_target = false;
            self.word_body(start + len, TokenKind::Plain);
            self.after_opener(text);
            return;
        }

        if self.in_case_pattern() && self.after_for_or_case == Stage::None {
            if text == "esac" {
                self.cases.pop();
                self.command_expected = false;
                self.push(TokenKind::Keyword, len);
            } else {
                self.word_body(start + len, TokenKind::Plain);
                self.after_opener(text);
            }
            return;
        }

        let in_command_position = self.command_expected;
        if std::mem::take(&mut self.after_time) && in_command_position && text.starts_with('-') {
            // `time -p pipeline`: the option belongs to `time`; the command is still to come.
            self.after_time = true;
            self.word_body(start + len, TokenKind::Option);
            return;
        }
        let mut kind = TokenKind::Plain;
        if self.after_for_or_case == Stage::In && text == "in" {
            kind = TokenKind::Keyword;
            self.after_for_or_case = Stage::None;
            if self.case_pending {
                self.case_pending = false;
                self.cases.push(Case {
                    pattern: true,
                    depth: self.nesting.len(),
                });
            }
        } else if self.after_for_or_case == Stage::Name && self.function_pending {
            // `function NAME [()]`: the body comes next, and it holds commands.
            self.after_for_or_case = Stage::None;
            self.function_pending = false;
            self.push(TokenKind::Variable, len);
            self.function_parens();
            return;
        } else if self.after_for_or_case == Stage::Name {
            self.after_for_or_case = Stage::In;
        } else if in_command_position && is_function_definition(text, &src[start + len..]) {
            // `NAME()`: a definition, not a run of NAME.
            self.after_for_or_case = Stage::None;
            self.push(TokenKind::Variable, len);
            self.function_parens();
            return;
        } else if in_command_position {
            self.after_for_or_case = Stage::None;
            if text == "[[" {
                // A conditional expression, up to its `]]`.
                self.push(TokenKind::Keyword, len);
                self.open(Closer::Brackets, false, true);
                return;
            } else if KEYWORDS_BEFORE_COMMAND.contains(&text) {
                kind = TokenKind::Keyword;
                self.after_time = text == "time";
            } else if KEYWORDS_CLOSING.contains(&text) {
                kind = TokenKind::Keyword;
                self.command_expected = false;
                if text == "esac" {
                    self.cases.pop();
                }
            } else if KEYWORDS_BEFORE_NAME.contains(&text) {
                kind = TokenKind::Keyword;
                self.command_expected = false;
                self.after_for_or_case = Stage::Name;
                self.case_pending = text == "case";
                self.function_pending = text == "function";
            } else if let Some(name_len) = assignment_name_len(text) {
                // `NAME=value` before the command: the command is still to come.
                self.push(TokenKind::Variable, name_len);
                self.push(TokenKind::Operator, 1);
                self.word_body(start + len, TokenKind::Plain);
                self.after_opener(text);
                return;
            } else {
                kind = TokenKind::Command;
                self.command_expected = false;
            }
        } else if text.starts_with('-') {
            kind = TokenKind::Option;
        }

        if kind == TokenKind::Keyword {
            self.push(kind, len);
        } else {
            self.word_body(start + len, kind);
        }
        self.after_opener(text);
    }

    /// Reads the optional `()` after a function's name. What follows is the body,
    /// so the next word (`{`, usually) is in command position.
    fn function_parens(&mut self) {
        let rest = self.rest();
        let blanks = |s: &str| s.len() - s.trim_start_matches([' ', '\t']).len();
        let open = blanks(rest);
        if rest[open..].starts_with('(') {
            let inner = blanks(&rest[open + 1..]);
            if rest[open + 1 + inner..].starts_with(')') {
                self.push(TokenKind::Plain, open);
                self.push(TokenKind::Operator, 1);
                self.push(TokenKind::Plain, inner);
                self.push(TokenKind::Operator, 1);
            }
        }
        self.command_expected = true;
    }

    /// A word ends right after `$(`, `$((` or a backquote (see `word_len`). What
    /// follows `$(` and an opening backquote is a command; arithmetic is not.
    /// When it closes, command position is back to what it was after this word.
    fn after_opener(&mut self, text: &str) {
        let after = self.command_expected;
        if text.ends_with("$((") {
            // Closed by `))`, read as two `)`.
            self.open(Closer::Paren, after, true);
            self.open(Closer::Paren, after, true);
        } else if text.ends_with("$(") {
            self.open(Closer::Paren, after, false);
        } else if text.ends_with('`') && !text.ends_with("\\`") {
            if self
                .nesting
                .last()
                .is_some_and(|n| n.closer == Closer::Backquote)
            {
                self.close(Closer::Backquote);
            } else {
                self.open(Closer::Backquote, after, false);
            }
        }
    }

    /// Colours the rest of a word up to `end`: quotes and expansions in their own
    /// colours, everything else in `base`.
    fn word_body(&mut self, end: usize, base: TokenKind) {
        while self.pos < end {
            let rest = &self.src[self.pos..end];
            let c = rest.chars().next().unwrap_or(' ');
            match c {
                '\'' => {
                    let len = rest[1..].find('\'').map_or(rest.len(), |i| i + 2);
                    self.push(TokenKind::String, len);
                }
                '"' => {
                    let len = double_quoted_len(rest);
                    self.push(TokenKind::String, len);
                }
                '$' => {
                    let len = expansion_len(rest);
                    if len == 1 {
                        self.push(base, 1);
                    } else {
                        self.push(TokenKind::Variable, len);
                    }
                }
                '`' => self.push(TokenKind::Variable, 1),
                '\\' => {
                    let len = 1 + rest[1..].chars().next().map_or(0, char::len_utf8);
                    self.push(base, len);
                }
                _ => self.push(base, c.len_utf8()),
            }
        }
    }
}

/// Length of a control or redirection operator at the start of `s`, if one is there.
fn operator_len(s: &str) -> Option<usize> {
    // Longest first, so that `&&` is not read as two `&`.
    const OPERATORS: &[&str] = &[
        "<<<", "<<-", "&>>", ";;&", "&&", "||", ";;", ";&", "|&", "<<", ">>", "<&", ">&", "<>",
        ">|", "&>", "|", "&", ";", "(", ")", "<", ">",
    ];
    // `<(` / `>(` is process substitution, which opens a command.
    if s.starts_with("<(") || s.starts_with(">(") {
        return Some(2);
    }
    OPERATORS
        .iter()
        .find(|op| s.starts_with(**op))
        .map(|op| op.len())
}

/// Length of the word at the start of `s`: up to unquoted whitespace or an operator.
fn word_len(s: &str) -> usize {
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().unwrap_or(' ');
        match c {
            ' ' | '\t' | '\r' | '\n' => break,
            '|' | '&' | ';' | '<' | '>' | '(' | ')' => break,
            '\'' => i += rest[1..].find('\'').map_or(rest.len(), |j| j + 2),
            '"' => i += double_quoted_len(rest),
            '$' if rest.starts_with("$(") => return i + expansion_len(rest),
            '$' => i += expansion_len(rest),
            '`' => return i + 1,
            '\\' => i += 1 + rest[1..].chars().next().map_or(0, char::len_utf8),
            _ => i += c.len_utf8(),
        }
    }
    i.min(s.len())
}

/// Length of the double-quoted string at the start of `s` (which starts with `"`).
/// Runs to the end of the text when it is not closed.
fn double_quoted_len(s: &str) -> usize {
    let mut chars = s.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return i + 1,
            _ => {}
        }
    }
    s.len()
}

/// Length of the expansion at the start of `s` (which starts with `$`).
///
/// `$(` and `$((` are only the opener: what is inside is a command (or
/// arithmetic) and is lexed as such, with the closing `)` read as an operator.
fn expansion_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    match bytes.get(1) {
        Some(b'{') => s.find('}').map_or(s.len(), |i| i + 1),
        Some(b'(') => {
            if bytes.get(2) == Some(&b'(') {
                3
            } else {
                2
            }
        }
        Some(b'\'') => 1 + s[1..][1..].find('\'').map_or(s.len() - 1, |i| i + 2),
        Some(b)
            if b.is_ascii_digit() || (b"?!#$@*-_".contains(b) && !is_name_byte(bytes.get(2))) =>
        {
            2
        }
        Some(b) if b.is_ascii_alphabetic() || *b == b'_' => {
            1 + s[1..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count()
        }
        _ => 1,
    }
}

fn is_name_byte(b: Option<&u8>) -> bool {
    b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Whether `word`, followed by `rest`, starts a function definition `NAME()`.
fn is_function_definition(word: &str, rest: &str) -> bool {
    let is_name = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'));
    let rest = rest.trim_start_matches([' ', '\t']);
    is_name
        && rest
            .strip_prefix('(')
            .is_some_and(|r| r.trim_start_matches([' ', '\t']).starts_with(')'))
}

/// Length of `NAME` when `word` is an assignment `NAME=...` (or `NAME+=...`, whose
/// `+` is then coloured with the name).
fn assignment_name_len(word: &str) -> Option<usize> {
    let eq = word.find('=')?;
    let name = word[..eq].strip_suffix('+').unwrap_or(&word[..eq]);
    let mut bytes = name.bytes();
    let first = bytes.next()?;
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    Some(eq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    /// The tokens as (kind, text), for readable assertions.
    fn lex(src: &str) -> Vec<(TokenKind, &str)> {
        highlight(src)
            .into_iter()
            .map(|t| (t.kind, &src[t.start..t.end]))
            .collect()
    }

    /// Only the coloured tokens.
    fn coloured(src: &str) -> Vec<(TokenKind, &str)> {
        lex(src).into_iter().filter(|(k, _)| *k != Plain).collect()
    }

    #[test]
    fn tokens_cover_the_text_end_to_end() {
        for src in [
            "",
            "ls -la",
            "echo \"unterminated",
            "echo 'x\ny' | wc -l\n# done\n",
            "cat <<EOF\nhello $USER\nEOF\necho after",
            "for f in *.md; do echo \"$f\"; done",
            "echo 日本語 $((1 + 2)) ${x:-デフォルト}",
            "\\",
            "$",
            "a=\"",
        ] {
            let tokens = highlight(src);
            let mut at = 0;
            for t in &tokens {
                assert_eq!(t.start, at, "gap or overlap in {src:?}: {tokens:?}");
                assert!(t.end > t.start, "empty token in {src:?}");
                assert!(src.is_char_boundary(t.start) && src.is_char_boundary(t.end));
                at = t.end;
            }
            assert_eq!(at, src.len(), "text not covered: {src:?}");
        }
    }

    /// Whatever the text, the tokens tile it and fall on character boundaries.
    /// Half-typed commands are the normal case in a document being edited.
    #[test]
    fn any_text_is_covered_without_panicking() {
        const PIECES: &[&str] = &[
            " ", "\n", "\t", "\\", "'", "\"", "`", "$", "{", "}", "(", ")", "((", "#", "|", "&",
            ";", "<", ">", "<<", "<<-", "-", "=", "a", "EOF", "in", "for", "do", "2", "é", "日",
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..5000 {
            let mut src = std::string::String::new();
            for _ in 0..(seed % 24) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                src.push_str(PIECES[(seed % PIECES.len() as u64) as usize]);
            }
            let mut at = 0;
            for t in highlight(&src) {
                assert_eq!(t.start, at, "gap or overlap in {src:?}");
                assert!(src.is_char_boundary(t.end), "split character in {src:?}");
                at = t.end;
            }
            assert_eq!(at, src.len(), "text not covered: {src:?}");
            seed = seed.wrapping_add(1);
        }
    }

    #[test]
    fn the_first_word_is_the_command_and_dashes_are_options() {
        assert_eq!(
            coloured("ls -la --color=auto src"),
            vec![(Command, "ls"), (Option, "-la"), (Option, "--color=auto")]
        );
    }

    #[test]
    fn every_command_of_a_pipeline_or_list_is_a_command() {
        assert_eq!(
            coloured("cat a | grep x && echo ok; date"),
            vec![
                (Command, "cat"),
                (Operator, "|"),
                (Command, "grep"),
                (Operator, "&&"),
                (Command, "echo"),
                (Operator, ";"),
                (Command, "date"),
            ]
        );
    }

    #[test]
    fn each_line_starts_a_command_but_a_continuation_does_not() {
        assert_eq!(
            coloured("echo a\nls \\\n  -l"),
            vec![(Command, "echo"), (Command, "ls"), (Option, "-l")]
        );
    }

    #[test]
    fn strings_variables_and_comments() {
        assert_eq!(
            coloured("echo 'a b' \"c $HOME\" $USER ${x} # note"),
            vec![
                (Command, "echo"),
                (String, "'a b'"),
                (String, "\"c $HOME\""),
                (Variable, "$USER"),
                (Variable, "${x}"),
                (Comment, "# note"),
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_word_is_not_a_comment() {
        assert_eq!(coloured("echo a#b"), vec![(Command, "echo")],);
    }

    #[test]
    fn redirections_do_not_start_a_command() {
        assert_eq!(
            coloured("make 2>&1 > log.txt"),
            vec![(Command, "make"), (Operator, "2>&"), (Operator, ">")]
        );
    }

    #[test]
    fn a_command_substitution_holds_a_command() {
        assert_eq!(
            coloured("echo $(date +%s)"),
            vec![
                (Command, "echo"),
                (Variable, "$("),
                (Command, "date"),
                (Operator, ")"),
            ]
        );
    }

    #[test]
    fn keywords_and_the_command_after_them() {
        assert_eq!(
            coloured("for f in a b; do echo $f; done"),
            vec![
                (Keyword, "for"),
                (Keyword, "in"),
                (Operator, ";"),
                (Keyword, "do"),
                (Command, "echo"),
                (Variable, "$f"),
                (Operator, ";"),
                (Keyword, "done"),
            ]
        );
        assert_eq!(
            coloured("if test -f x; then rm x; fi"),
            vec![
                (Keyword, "if"),
                (Command, "test"),
                (Option, "-f"),
                (Operator, ";"),
                (Keyword, "then"),
                (Command, "rm"),
                (Operator, ";"),
                (Keyword, "fi"),
            ]
        );
    }

    #[test]
    fn a_keyword_is_only_a_keyword_in_command_position() {
        assert_eq!(coloured("echo if do done"), vec![(Command, "echo")]);
    }

    #[test]
    fn an_assignment_before_the_command() {
        assert_eq!(
            coloured("LANG=C sort file"),
            vec![(Variable, "LANG"), (Operator, "="), (Command, "sort")]
        );
    }

    #[test]
    fn a_heredoc_body_is_a_string_and_not_commands() {
        assert_eq!(
            coloured("cat <<'EOF' > out\nrm -rf x\nEOF\nls"),
            vec![
                (Command, "cat"),
                (Operator, "<<"),
                (String, "'EOF'"),
                (Operator, ">"),
                (String, "rm -rf x\n"),
                (Operator, "EOF\n"),
                (Command, "ls"),
            ]
        );
    }

    #[test]
    fn an_indented_heredoc_closes_on_a_tab_indented_delimiter() {
        assert_eq!(
            coloured("cat <<-END\n\tbody\n\tEND\necho"),
            vec![
                (Command, "cat"),
                (Operator, "<<-"),
                (String, "END"),
                (String, "\tbody\n"),
                (Operator, "\tEND\n"),
                (Command, "echo"),
            ]
        );
    }

    #[test]
    fn backquotes_hold_a_command() {
        assert_eq!(
            coloured("echo `date -u` x"),
            vec![
                (Command, "echo"),
                (Variable, "`"),
                (Command, "date"),
                (Option, "-u"),
                (Variable, "`"),
            ]
        );
    }

    #[test]
    fn a_leading_redirection_leaves_the_command_to_come() {
        assert_eq!(
            coloured("> out.txt echo hi; 2>/dev/null ls -a"),
            vec![
                (Operator, ">"),
                (Command, "echo"),
                (Operator, ";"),
                (Operator, "2>"),
                (Command, "ls"),
                (Option, "-a"),
            ]
        );
    }

    #[test]
    fn process_substitution_holds_a_command() {
        assert_eq!(
            coloured("diff <(sort a) <(sort b) -u"),
            vec![
                (Command, "diff"),
                (Operator, "<("),
                (Command, "sort"),
                (Operator, ")"),
                (Operator, "<("),
                (Command, "sort"),
                (Operator, ")"),
                (Option, "-u"),
            ]
        );
    }

    #[test]
    fn after_a_substitution_command_position_is_what_it_was() {
        // An assignment's value: the command is still to come.
        assert_eq!(
            coloured("STAMP=$(date) env -i"),
            vec![
                (Variable, "STAMP"),
                (Operator, "="),
                (Variable, "$("),
                (Command, "date"),
                (Operator, ")"),
                (Command, "env"),
                (Option, "-i"),
            ]
        );
        assert_eq!(
            coloured("A=`date` env"),
            vec![
                (Variable, "A"),
                (Operator, "="),
                (Variable, "`"),
                (Command, "date"),
                (Variable, "`"),
                (Command, "env"),
            ]
        );
        // An argument: the rest are arguments too.
        assert_eq!(
            coloured("echo $(date) x -n"),
            vec![
                (Command, "echo"),
                (Variable, "$("),
                (Command, "date"),
                (Operator, ")"),
                (Option, "-n"),
            ]
        );
        // A subshell is followed by operators, not a command.
        assert_eq!(
            coloured("(cd src; ls) | wc"),
            vec![
                (Operator, "("),
                (Command, "cd"),
                (Operator, ";"),
                (Command, "ls"),
                (Operator, ")"),
                (Operator, "|"),
                (Command, "wc"),
            ]
        );
    }

    #[test]
    fn case_patterns_are_not_commands_but_their_arms_are() {
        assert_eq!(
            coloured("case \"$x\" in a|b) echo one;; (c) printf two;; esac; ls"),
            vec![
                (Keyword, "case"),
                (String, "\"$x\""),
                (Keyword, "in"),
                (Operator, "|"),
                (Operator, ")"),
                (Command, "echo"),
                (Operator, ";;"),
                (Operator, "("),
                (Operator, ")"),
                (Command, "printf"),
                (Operator, ";;"),
                (Keyword, "esac"),
                (Operator, ";"),
                (Command, "ls"),
            ]
        );
        // Over several lines, and with the last arm left without `;;`.
        assert_eq!(
            coloured("case $1 in\n  start)\n    run -d\n    ;;\n  *) help\nesac\ndate"),
            vec![
                (Keyword, "case"),
                (Variable, "$1"),
                (Keyword, "in"),
                (Operator, ")"),
                (Command, "run"),
                (Option, "-d"),
                (Operator, ";;"),
                (Operator, ")"),
                (Command, "help"),
                (Keyword, "esac"),
                (Command, "date"),
            ]
        );
    }

    #[test]
    fn a_word_goes_on_after_a_substitution_inside_it() {
        assert_eq!(
            coloured("PATH=$(pwd)/bin:`pwd`/x env -i"),
            vec![
                (Variable, "PATH"),
                (Operator, "="),
                (Variable, "$("),
                (Command, "pwd"),
                (Operator, ")"),
                (Variable, "`"),
                (Command, "pwd"),
                (Variable, "`"),
                (Command, "env"),
                (Option, "-i"),
            ]
        );
        assert_eq!(
            coloured("echo $(date)#x"),
            vec![
                (Command, "echo"),
                (Variable, "$("),
                (Command, "date"),
                (Operator, ")"),
            ]
        );
    }

    #[test]
    fn a_function_definition_is_not_a_run() {
        let body = [
            (Keyword, "{"),
            (Command, "echo"),
            (Operator, ";"),
            (Keyword, "}"),
        ];
        let with = |head: &[(TokenKind, &'static str)]| {
            head.iter().copied().chain(body).collect::<Vec<_>>()
        };
        assert_eq!(
            coloured("greet() { echo hi; }"),
            with(&[(Variable, "greet"), (Operator, "()")])
        );
        assert_eq!(
            coloured("greet ( ) { echo hi; }"),
            with(&[(Variable, "greet"), (Operator, "("), (Operator, ")")])
        );
        assert_eq!(
            coloured("function greet { echo hi; }"),
            with(&[(Keyword, "function"), (Variable, "greet")])
        );
        assert_eq!(
            coloured("function greet() { echo hi; }"),
            with(&[(Keyword, "function"), (Variable, "greet"), (Operator, "()")])
        );
        // A subshell straight after a command name is still a subshell.
        assert_eq!(
            coloured("echo (x)"),
            vec![
                (Command, "echo"),
                (Operator, "("),
                (Command, "x"),
                (Operator, ")")
            ]
        );
    }

    #[test]
    fn options_of_time_come_before_the_command() {
        assert_eq!(
            coloured("time -p sleep 1"),
            vec![(Keyword, "time"), (Option, "-p"), (Command, "sleep")]
        );
    }

    #[test]
    fn a_substitution_in_a_case_pattern_does_not_end_it() {
        assert_eq!(
            coloured("case x in $(printf x)) echo yes;; esac"),
            vec![
                (Keyword, "case"),
                (Keyword, "in"),
                (Variable, "$("),
                (Command, "printf"),
                (Operator, "))"),
                (Command, "echo"),
                (Operator, ";;"),
                (Keyword, "esac"),
            ]
        );
    }

    #[test]
    fn a_process_substitution_in_a_case_pattern_does_not_end_it() {
        assert_eq!(
            coloured("case x in <(printf x)) echo yes;; esac"),
            vec![
                (Keyword, "case"),
                (Keyword, "in"),
                (Operator, "<("),
                (Command, "printf"),
                (Operator, "))"),
                (Command, "echo"),
                (Operator, ";;"),
                (Keyword, "esac"),
            ]
        );
    }

    #[test]
    fn a_conditional_expression_holds_no_commands() {
        assert_eq!(
            coloured("[[ \"$x\" == y && -f file ]] && echo ok"),
            vec![
                (Keyword, "[["),
                (String, "\"$x\""),
                (Operator, "&&"),
                (Option, "-f"),
                (Keyword, "]]"),
                (Operator, "&&"),
                (Command, "echo"),
            ]
        );
    }

    #[test]
    fn an_array_value_holds_no_commands() {
        assert_eq!(
            coloured("items=(one\n  two) ; declare -a more+=(three)"),
            vec![
                (Variable, "items"),
                (Operator, "=("),
                (Operator, ")"),
                (Operator, ";"),
                (Command, "declare"),
                (Option, "-a"),
                (Operator, "("),
                (Operator, ")"),
            ]
        );
    }

    #[test]
    fn a_newline_inside_arithmetic_is_not_a_new_command() {
        assert_eq!(
            coloured("echo $((1 +\n total)) x\nls"),
            vec![
                (Command, "echo"),
                (Variable, "$(("),
                (Operator, "))"),
                (Command, "ls"),
            ]
        );
    }

    #[test]
    fn a_substitution_as_the_case_subject_still_reaches_in() {
        assert_eq!(
            coloured("case $(printf y) in y) echo yes;; esac"),
            vec![
                (Keyword, "case"),
                (Variable, "$("),
                (Command, "printf"),
                (Operator, ")"),
                (Keyword, "in"),
                (Operator, ")"),
                (Command, "echo"),
                (Operator, ";;"),
                (Keyword, "esac"),
            ]
        );
    }

    #[test]
    fn grouping_inside_arithmetic_is_not_a_command() {
        assert_eq!(
            coloured("echo $(( (total + 1) * 2 )) x"),
            vec![
                (Command, "echo"),
                (Variable, "$(("),
                (Operator, "("),
                (Operator, ")"),
                (Operator, "))"),
            ]
        );
        assert_eq!(
            coloured("for ((i = 0; i < 3; i++)); do date; done"),
            vec![
                (Keyword, "for"),
                (Operator, "(("),
                (Operator, ";"),
                (Operator, "<"),
                (Operator, ";"),
                (Operator, "));"),
                (Keyword, "do"),
                (Command, "date"),
                (Operator, ";"),
                (Keyword, "done"),
            ]
        );
        assert_eq!(
            coloured("(( (n + 1) > 2 )) && echo big"),
            vec![
                (Operator, "(("),
                (Operator, "("),
                (Operator, ")"),
                (Operator, ">"),
                (Operator, "))"),
                (Operator, "&&"),
                (Command, "echo"),
            ]
        );
    }

    #[test]
    fn arithmetic_is_not_a_command() {
        assert_eq!(
            coloured("echo $((1 + 2))"),
            vec![(Command, "echo"), (Variable, "$(("), (Operator, "))")]
        );
    }
}
