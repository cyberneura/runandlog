# Run and Log

A tool that runs the shell commands written in a Markdown file and writes the
results back into that same file.

It exists for a loop that comes up constantly: an AI agent asks a person to run a
command, the person runs it, and the agent reads the result. Run and Log keeps
that exchange inside a single file. Like an IPython notebook, command cells and
their results sit next to each other -- except the document stays plain Markdown.

## Install

```shell
cargo install --path crates/runandlog-cli
```

This installs the `runandlog` binary.

## Usage

```shell
runandlog exam.md              # open the TUI
runandlog exam.md --list       # list the cells
runandlog exam.md --run 2      # run only cell 2 (may be repeated)
runandlog exam.md --run-all    # run every cell in order
runandlog exam.md --gui        # open the desktop app
```

### Options

| Option | Default | Description |
|---|---|---|
| `--max-inline-lines <N>` | 50 | Write output to a separate file once it exceeds this many lines |
| `--shell <PATH>` | `SHELL` environment variable | Shell the commands are handed to |
| `--cwd <DIR>` | Directory of the Markdown file | Working directory for the commands |
| `--timeout <SECONDS>` | unlimited | Time limit per cell. On expiry the whole process group is killed |
| `--gui` | - | Open in the desktop app (GUI) |

### TUI keys

| Key | Action |
|---|---|
| `j` / `k` (`↓` / `↑`) | Move the selection between cells |
| `Enter` / `r` | Run the selected cell |
| `a` | Run every cell in order (stops if a result cannot be written back) |
| `g` / `G` | Jump to the first / last cell |
| `PageUp` / `PageDown` | Scroll |
| `R` | Reload the file |
| `q` / `Esc` / `Ctrl-C` | Quit (while a command is running, quitting waits for it to finish) |

### The desktop app

`--gui` opens a window showing the same document the TUI shows: the file path,
then every cell with its command, a `▶ Run` button, and the result of the last
run. **Run all** runs every cell in order, **Reload** picks up edits made in an
external editor.

Results are written back to the Markdown exactly as they are from the CLI and the
TUI: the same markers, the same separate-file handling, the same refusal to write
when the file changed while a command was running. Parsing, running and writing
all go through `runandlog-core` and the same `session` module, so the two front
ends cannot drift apart.

The GUI is a [Tauri](https://tauri.app/) app and needs the system webview
(`webkit2gtk-4.1` and `gtk3` on Linux) at build time. It is on by default; to
build the CLI and TUI alone, drop it:

```shell
cargo build --no-default-features
```

Such a build still accepts `--gui`, but reports that this build has no GUI rather
than opening a window.

## Writing the Markdown

### Blocks that get run

A fenced code block becomes a cell when its info string is `shell`, `sh`, `bash`,
or `zsh`. Blocks in any other language are left alone.

````markdown
```shell
date
```
````

Several lines in one block are run together in a single shell invocation.

````markdown
```shell
ls /opt
ls /tmp
```
````

### Results

Running a cell writes a result block directly after it.

````markdown
```shell
date
```

<!-- runandlog:begin -->
Ran result: 2026-08-14 09:53:32 (exit 0, 0.02s, 1 lines)

```text
Fri Aug 14 09:53:32 JST 2026
```
<!-- runandlog:end -->
````

The result is wrapped in HTML comment markers. They are valid Markdown, they do
not show up when rendered, and they let a re-run replace the previous result
reliably -- so no matter how many times a cell runs, exactly one result block
remains.

### Sending the result to a separate file

Output longer than `--max-inline-lines` (50 by default) is written to
`<markdown name>-result-<cell number>.txt`, and only a link to it goes into the
Markdown.

To always use a separate file regardless of length, designate the destination up
front. There are two ways to do it.

Put a `Result:` paragraph right after the block, with the destination as a link:

````markdown
```shell
date
```

Result:
[date-command-result.txt](date-command-result.txt)
````

Or write `out=` in the fence info string:

````markdown
```shell out=date-command-result.txt
date
```
````

The destination must stay under the directory holding the Markdown file. An
absolute path, one containing `..`, and one naming a directory that already
exists are all ignored, and the output goes where it normally would (inline, or
into an auto-numbered file).

A destination containing spaces is written as `[a b.txt](<a b.txt>)`, since a
bare Markdown link target cannot contain them.

## Layout

| Crate | Role |
|---|---|
| `crates/runandlog-core` | Markdown parsing, command execution, result formatting. A pure core with no file IO |
| `crates/runandlog-cli` | The `runandlog` binary: CLI, TUI, GUI, and file IO |

All three front ends sit on the same `runandlog-core` and the same `session`
module, so that the parsing rules and the write-back rules cannot drift apart.

## Development

```shell
cargo test          # all tests
cargo clippy --all-targets
cargo fmt --all
```

`examples/exam.md` is a sample for trying things out. **Running it rewrites the
file**, so work on a copy.

```shell
cp examples/exam.md /tmp/exam.md
cargo run -p runandlog-cli -- /tmp/exam.md
```
