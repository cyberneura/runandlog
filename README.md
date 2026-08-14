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
```

### Options

| Option | Default | Description |
|---|---|---|
| `--max-inline-lines <N>` | 50 | Write output to a separate file once it exceeds this many lines |
| `--shell <PATH>` | `SHELL` environment variable | Shell the commands are handed to |
| `--cwd <DIR>` | Directory of the Markdown file | Working directory for the commands |
| `--timeout <SECONDS>` | unlimited | Time limit per cell. On expiry the whole process group is killed |
| `--gui` | - | Open in the desktop app (not implemented yet) |

### TUI keys

| Key | Action |
|---|---|
| `j` / `k` (`↓` / `↑`) | Move the selection between cells |
| `Enter` / `r` | Run the selected cell |
| `a` | Run every cell in order |
| `g` / `G` | Jump to the first / last cell |
| `PageUp` / `PageDown` | Scroll |
| `R` | Reload the file |
| `q` / `Esc` / `Ctrl-C` | Quit (while a command is running, quitting waits for it to finish) |

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
absolute path, or one containing `..`, is ignored and the output goes where it
normally would (inline, or into an auto-numbered file).

## Layout

| Crate | Role |
|---|---|
| `crates/runandlog-core` | Markdown parsing, command execution, result formatting. A pure core with no file IO |
| `crates/runandlog-cli` | The `runandlog` binary: CLI, TUI, and file IO |

A GUI (desktop app) will be added later on top of the same `runandlog-core`. The
core is always shared, so that the parsing rules cannot drift apart.

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
