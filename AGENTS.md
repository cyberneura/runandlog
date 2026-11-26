# Run and Log development notes

A tool that runs the shell commands inside a Markdown file and writes the results back into the same Markdown.
See README.md for the specification and usage. This file records only development decisions and caveats.

## Language policy

This is a public repository, so **the README, code comments, UI strings, error messages, and all documentation
(including this AGENTS.md / CLAUDE.md) are written in English** (instructed in CYBERNEURA-DEV-442; extended to
the agent-facing development notes in CYBERNEURA-DEV-1377).

The one exception is `結果:`, accepted by `parse::is_designation_head`. It is not UI output but
**a spelling accepted as input**, kept so that Markdown written in Japanese can be read.

## Design essentials

- **The core does no IO.** `runandlog-core` is responsible only for parsing (`parse`), execution (`exec`), and formatting (`render`);
  reading and writing files is done by `session` in the `runandlog` crate (`crates/runandlog-cli/`). This lets the TUI, non-interactive runs,
  and the future GUI share the same core.
- **Write-back is done only by range replacement.** `parse` keeps results as byte offsets, and `splice`
  replaces the corresponding range. Because the Markdown is never rebuilt, the original formatting stays intact.
- **Results are wrapped in HTML comment markers.** This lets a re-run reliably identify and replace
  the previous result. Without markers, results would keep being appended, or the replacement would swallow and break the body text.
- **The first line of a result block carries the cell number** (CYBERNEURA-DEV-736).
  `Ran result: cell 3 - <date/time> (...)`. This is the only clue a reader (especially an AI agent) has to
  identify a cell: **headings are hand-written, so they may have no number, may not map
  1:1 to cells, and drift as soon as a cell is inserted.** In a real incident, an agent
  numbered the headings from 0, and the conversation continued off by one from the numbers the user was looking at.
  - **Numbers are 1-based**, the same as `--list` / `--run N` / TUI / GUI.
    **Do not make them 0-based** — it is humans who say the numbers out loud, and we do not bend
    the human-facing UI for the convenience of AI readers.
  - **Blocks are identified by the markers only** (the first line is not looked at). Old result blocks
    without a number are replaced as-is. The regression test is
    `render::tests::a_result_without_a_number_is_replaced_whole`.
  - **On every write-back, the numbers in the result blocks of the other cells are realigned too**
    (`renumber_result`, raised in Codex review). The number is positional, so inserting one cell
    shifts every cell after it. Already-written blocks keep the number they had when they ran, so
    **we would be telling readers "look at this number" while it silently becomes a lie** — destroying
    the very purpose of adding this line. Running any one thing in the file realigns them.
  - **Only the first line of a result block is rewritten, and only when it looks like the form we wrote.**
    The inside of the markers belongs to runandlog, so correcting the number is not rewriting the
    user's prose, but a line that was edited by hand is left as-is rather than guessed at.
    **For inserting a number where none exists, a prefix match on `Ran result: ` is not enough**
    (hand-written lines such as `Ran result: imported from CI` start the same way), so we also check
    that what follows has the shape of the timestamp that `summary_line` writes
    (`starts_with_timestamp`, raised in Codex review). Where `cell <digits> - ` is already
    present, that spelling itself is the signature, so judging by shape alone is fine there.
- **stdout and stderr are received through a single pipe** (`std::io::pipe`). Reading them separately and
  concatenating breaks the output order and makes it unreadable as a log.

## Implementation caveats

- `exec::run` starts the command in a dedicated session (= a dedicated process group) and kills the whole group on timeout.
  Killing only the shell leaves the commands the shell started running, and the pipe stays open,
  so reading the output never finishes. Without a timeout this protection does not apply, so reading is
  cut off after `DRAIN_GRACE` and whatever was read becomes the result.
- **After spawn, drop the `Command` to release the write ends of the pipe.** `Command` keeps
  holding the write ends passed to `stdout` / `stderr`, so unless it is dropped a copy remains in the parent,
  and the reader thread never gets EOF even after the child exits. As a result **every run waits the full
  `DRAIN_GRACE` (300ms), and the reported duration is inflated by that much**. The test guarding against
  regression is `exec::tests::a_fast_command_does_not_wait_for_the_drain_grace`.
- `ExecOutcome::duration` is fixed at the moment the process exits. The drain wait is not part of the
  command's execution time, so it is not included.
- Output capture has a limit (`ExecOptions::max_output_bytes`, default 8 MiB). Past the limit, the output is
  just discarded while reading continues. If we stopped reading, the pipe would fill up and the command would stall.
- **Do not trust output destinations given by `out=` or `Result:`.** The Markdown content is untrusted input
  that may be written by an AI agent, so specifications containing absolute paths or `..` are rejected by
  `render::is_inside_dir`, preventing writes outside the directory containing the Markdown.
- When searching for a result block's end marker, code fences are skipped (`parse::find_end_marker`).
  If the command output itself contains a line identical to the end marker, the extent of the result block would otherwise be misidentified.
- Writes to the Markdown go through a temporary file and rename (`session::write_atomically`).
  This keeps the original file from being left half-written if interrupted mid-run. Just before writing,
  `refresh_before_write` re-reads the file so that external edits made during the run are not silently clobbered.
- TUI runs are handed to a worker thread. Running synchronously would freeze the screen during long commands.
- **Ctrl-C is caught by the app, which explicitly kills the process group** (`Canceller`). Because the command
  runs in its own process group, the SIGINT that the terminal sends to the foreground process group
  reaches only runandlog. Without a handler, runandlog alone dies and the command survives,
  taking with it the output the user was waiting for.
  - Non-interactive runs (`main::catch_interrupts`) catch SIGINT / SIGTERM in a handler, record **the received
    signal number**, and a watcher thread calls `Canceller::cancel`. **Do not kill inside the handler.**
    `cancel` takes a lock, so it cannot be called from the handler, and the handler only receives the signal number anyway,
    so reaching the `Canceller` would require re-homing it in a static.
    The exit code is `128 + signo` (130 for SIGINT, 143 for SIGTERM). A second signal
    calls `_exit` without waiting for the write-back.
  - **What is checked before and after each cell is `INTERRUPTED_BY`, written directly by the handler, not the `Canceller`.**
    The watcher thread wakes only every 50ms, so right after a signal arrives the canceller does not know anything yet.
    Checking the canceller would start the next cell even though the signal has already arrived, causing the overwrite described below.
    The problem of the exit code not becoming 130 / 143 when the signal arrives right after the last cell has the same cause.
  - **The cancel flag is also checked before running a cell.** If Ctrl-C arrives while the previous cell is being written back,
    the next cell would be cancelled right after it starts, and **a zero-output result would clobber that cell's previous result**.
    Results would be lost to a run nobody asked for. The GUI's `run_all` checks
    `stop_requested` before each cell for the same reason.
  - The TUI is in raw mode, so SIGINT itself never arrives; Ctrl-C is received as a key event and triggers cancel.
    `q` / `Esc` keep their existing behaviour: "quit after it finishes".
  - `Canceller::cancel` touches the flag then the pid; `run` touches the pid then the flag. Even if
    cancel arrives in the middle of spawn, one of them always sees the other, so nothing is missed.
  - **The pid is a `Mutex<i32>`, not an atomic.** The killing side ("read the id → kill") and
    the reaping side ("reap → discard the id") must exclude each other.
    A thread can be paused for an arbitrary time between reading and killing, so if the child is reaped in between and
    the pid is reused, **an unrelated process group gets killed**. The lock is held for one
    poll every 20ms, while the polling for cancel requests itself stays an AtomicBool.
    The price is that `cancel` cannot be called from a signal handler (and it is not).
  - **A Stop between the shell exiting and the drain (up to 300ms) finishing kills nothing.**
    After the leader is reaped, the process group survives as long as descendants remain, but at that point
    we cannot tell whether "the group is gone and its number was reused" or "the original descendants still belong to it".
    Descendants holding the pipe survive.
  - **Cancelled runs also write back their results.** Keeping the output produced so far is the point of cancelling.

## Commands that ask for a password (askpass, CYBERNEURA-DEV-879)

- **Commands are started in a new session via `setsid`** (`exec::detach_from_terminal`).
  With only the previous `process_group(0)`, the controlling terminal stayed the same as runandlog's and the command merely became
  a "non-foreground process group", so **programs that read `/dev/tty` directly (sudo does so regardless
  of stdin) were stopped by SIGTTIN and waited forever until cancelled**. With a separate session there is
  no controlling terminal, and the open fails immediately with ENXIO. The regression test is
  `exec::tests::the_command_has_no_terminal_to_wait_on` (it makes no difference unless run with a terminal;
  verified with `script -qec 'cargo test ...' /dev/null`).
  A session leader is also a process group leader, so killing the group still works.
- **Instead, an askpass helper is provided** (`crates/runandlog-cli/src/askpass.rs`).
  The helper is runandlog itself, started through a symlink named `runandlog-askpass`
  placed in a temporary directory (0700). `main` dispatches on the file name of argv[0]
  **before clap** (what the helper receives is a prompt, not Markdown).
  The helper sends the prompt to a Unix socket in the same directory, and the runandlog that started the command
  asks through its front end (non-interactive = `/dev/tty` with echo off, TUI = status line, GUI = dialog) and returns the answer.
  - The variables set are `SUDO_ASKPASS` / `SSH_ASKPASS` / `GIT_ASKPASS`, plus
    `RUNANDLOG_ASKPASS_SOCKET` for the socket location. **An askpass the user has already set is not overwritten.**
    **Only `SSH_ASKPASS_REQUIRE=force` is always set, regardless of whether the user has an askpass
    or an existing value** (two Codex review findings, confirmed in `readpass.c`). `prefer` keeps the
    "if there is a display" condition, so it has no effect on macOS or over SSH, and `never` just means
    "nobody can be asked" for a command without a terminal. Even when the user's askpass is kept,
    ssh will not call it without `force`.
  - **sudo does not use the helper without `-A` unless `DISPLAY` / `WAYLAND_DISPLAY` is set**
    (sudo's `tgetpass.c`). On macOS and over SSH the user has no choice but to write `sudo -A`.
    Faking `DISPLAY` would break programs that try to connect to X, so we do not do it.
  - **The helper is not offered in non-interactive runs without a terminal (cron / CI).** Offering it would turn sudo's error into
    "askpass failed", hiding the real cause (there is neither a terminal nor an askpass).
  - **The prompt checks `gone` every `PROMPT_POLL` (100ms) and gives up once it is true**
    (the second argument of `Prompter`). `gone` means "the helper has disappeared (the command finished, timed out,
    or was Stopped)" or "the `Askpass` is being dropped". Whether the helper is alive is detected by
    **whether reading the socket hits EOF**. For that, the helper sends the prompt terminated by NUL and waits for the answer
    **without closing its write side** (the listener reads up to the NUL). Writing heartbeats and watching for EPIPE does not work on macOS
    (writes keep succeeding after the peer has closed; only the macOS leg of CI failed). Non-interactive runs additionally
    check `interrupt_requested` (Ctrl-C).
  - **Dropping `Askpass` waits for the prompt to finish** (raised in Codex review). A terminal prompt keeps echo off
    until it returns, so if the process exited without waiting, the user's terminal would be left with echo off.
    Terminal settings are restored by the Drop of `RestoreTerminal`.
    Known hole: pressing Ctrl-C twice within 100ms during a prompt can make the second `_exit`
    run before the restore (there is no mechanism to restore termios from the signal handler).
  - **One prompt at a time** (socket accepts are processed serially). The front end can show only one.
  - **When the command finishes, any visible prompt is discarded** (`decline_prompts` in the TUI,
    `abandon_passwords` in the GUI). Dropping the sender makes the helper side see "declined".
    **The TUI declines not only the prompt being shown but also the requests left in the `password_requests` queue**
    (raised in Codex review). Requests that arrive after the last tick stay in the queue, and with run all the next cell
    starts immediately, so the next tick would display them as the next cell's question
    (thanks to the token, the input does not reach the old helper, but the display would be a lie).
    **Like the GUI, the TUI holds whether it is accepting (`PasswordQueue::accepting`) under the same lock as sending**
    (raised in Codex review). The listener checks the run, releases the lock, then calls the prompter, so
    if the run ends right after the check, a request can be queued after the drain. Ending a run means "take the lock,
    close, then drain"; starting opens it before `start_run()`.
    **On registration, `gone()` is also checked under that lock** (`PasswordQueue::submit` in the TUI,
    `request_password` in the GUI; raised by Codex CLI). "It is open" is not enough — while the request is in flight,
    its own run's queue may have been closed and reopened for the next run, in which case what is open is
    the next run's queue. `gone()` is true once its own run has finished, which distinguishes the two.
    The GUI additionally **performs the `emit` under the same lock** (`show_password_request`). If
    `abandon_passwords` ran between registration and display, the registration would be gone but the dialog alone would appear under the next cell. The GUI
    front end closes the dialog on `finished` and in the finally of `runCell` / `runAll`.
    The GUI assigns ids to requests so that a late answer to a finished request does not hit the next one.
  - **Requests carry a run number, and only those of the run in progress are answered** (two Codex review findings).
    `Session::start_run()`, through a run hook (`Askpass::hook`), adds
    `RUNANDLOG_ASKPASS_RUN=<n>` to the environment for each run, and the helper sends it as one line before the prompt.
    The `Run` returned by `start_run()` contains a token (`RunInProgress`), and
    **the `Run` is dropped when the command finishes (before the write-back)** — that ends the run,
    `current_run` goes back to 0, and all later requests are declined. Merely advancing the number would mean
    that when leftovers ask between cells, during write-back, or after the last cell, non-interactive runs (terminal)
    would answer, because they have no "decline unless running" gate. Processes left in the background by the previous cell
    still carry the previous number, so they are declined even if they ask while the next cell is running
    (answering would show it as "cell B's question" and hand the input to cell A's leftovers).
    **`start_run()` is called exactly once per run, and the `Run` is dropped on the worker side right after
    the command finishes** (inside the worker thread for TUI / GUI, inside `run_cell_streaming` for non-interactive).
    The token's Drop sets it to 0 only "if it is still its own run"
    (so that an old token dropped late does not end the next run).
    **`current_run` is a `Mutex<u64>`, not an atomic** (raised in Codex review). Answers are written
    under this lock, so there is no window where "the check passed but the run ended before writing" —
    by the time the token's Drop returns, none of that run's answers is in flight. Writes have a
    timeout (`PROMPT_POLL`), so a helper that stopped reading cannot hold the lock for long.
    Checking only on arrival cannot close the "run ends right after the check" race (raised by Codex CLI), so
    `gone` also checks the number to take down a visible prompt, and the number is checked once more right before returning the answer.
    The remaining hole is that a taken-down prompt may stay displayed in the front end (the TUI's `password` / the GUI's dialog)
    — the input goes nowhere, and it disappears when that cell finishes.
  - **Requests that arrive when nothing is running are declined immediately** (e.g. child processes a command left in the background).
    There is no way to show which cell is asking. The GUI holds whether it is accepting (`Passwords::accepting`) under the
    **same lock** as request registration (raised in Codex review: if it registered after checking `busy`,
    `abandon_passwords` could run in between and leave a request from a finished run behind. `busy` is also true
    during write-back and between cells, so it cannot serve as the "running" check either).
  - **The GUI accepts requests only after the front end's subscription has succeeded** (`listen_for_passwords`, raised in Codex
    review). In the degraded mode where subscription failed, nobody would see a request, so it is declined immediately.
  - Answers go into neither the output nor the Markdown. What appears on the command's stdout is the helper's
    stdout, which is the pipe read by sudo and the like, not the pipe captured by runandlog.
- "Terminal input other than passwords", such as `y/N` confirmations or full-screen editors, is out of scope. Running under a pty and
  relaying input was considered, but it was not adopted because of the conflict with the "stdout and stderr through one pipe" design
  and the impact of control sequences and `\r\n` mixing into the output.

## Known limitation: non-unix platforms are not supported

The process management in `exec` **only holds up on unix**. On Windows,
`kill_process_group` cannot have a process group and can kill only the shell, so
**timeouts do not work**, and surviving descendants keep holding the pipe. Moreover,
`make_reads_interruptible` is a no-op, so in that state the reader thread and the
pipe handles leak for the lifetime of the descendants (pointed out in the Codex review for
CYBERNEURA-DEV-442).

Fixing it requires job objects and overlapped reads on the Windows side, but **this development environment can
neither build nor verify that**, so it has not been touched. If Windows is to be targeted, first
set up CI that can verify on that environment.

## GUI (Tauri)

`--gui` opens a Tauri window (implemented in CYBERNEURA-DEV-445).

- **Use `runandlog-core` and `session` as they are.** The parse, run, and write-back paths are
  exactly the same as the TUI and non-interactive runs, and the GUI side has no knowledge of the Markdown specification at all.
  Branching here would make keeping up with the specification fall apart, so do not add GUI-specific parsing.
- **`gui` is a cargo feature** (on by default). Tauri requires webkit2gtk / gtk3 to build, so
  distributors who only want the CLI / TUI can drop it with `--no-default-features`. Even in builds without it,
  `--gui` is accepted and answers "this build has no GUI" (it is not left as an unknown clap flag).
- **Runs are handed to a blocking worker** (`tauri::async_runtime::spawn_blocking`).
  `runandlog_core::run` blocks until completion, so calling it directly on the command thread would freeze the
  window for as long as the command runs. Same reason the TUI uses a worker thread.
- **Do not hold the `Session` lock across an `await`.** Holding it across makes the `MutexGuard` non-Send and
  compilation fails, and even if it compiled, it would block reads of the screen for the whole run.
- **Concurrent runs are limited to one** (the `busy` flag). If two runs wrote back the same file,
  each would treat the other's write as an "external edit" and `refresh_before_write` would fail for both.
- **Reload takes the same `busy` flag too** (flagged as Critical in Codex review).
  `apply_outcome` detects external edits by comparing "the document held before the run" with the actual file,
  but reloading during a run replaces that baseline with the current contents on disk, **so the comparison passes**.
  The guard is disabled by the very thing it is meant to protect, and an old command's result could
  be written back into a different cell. The regression test is `reloading_is_refused_while_a_command_is_running`.
- **Stop gets a fresh `Canceller` for each run** (`GuiState::arm` / `stop`). Reusing one would
  leave the flag set once Stop was pressed, and the next command would be cancelled immediately. When a run
  finishes, it is removed with `arm(None)`, so that a late-arriving Stop does not hit the next command.
  `run_all` stops at a cancelled cell (`RunReport::cancelled`).
- **The GUI's operation state (`busy` / `stop_requested` / `Canceller`) is handled by a single `Mutex<Operation>`.**
  With separate atomics, the end of one operation and the start of the next could slip in between "is it running",
  "remember the Stop", and "kill the command", and **the previous operation's Stop would hit the first cell of the next operation**.
- **A Stop while the `Canceller` is detached is caught by the `stop_requested` flag.** The `Canceller` exists
  only while a command is running, but the front end, after enabling Stop on `runandlog://started`,
  keeps it enabled until the operation finishes (it can be pressed between cells too). A Stop pressed between cells of `run_all` (a window
  on the order of ms that includes the real IO of write-back) would, without the flag, be recorded nowhere, and **the batch would keep running
  while showing "stopped"**.
  The flag is cleared by `acquire` (the start of an operation).
  That the batch ended due to Stop is returned in `BatchReport::stopped`. It can end without a single
  cancelled run, so it cannot be read from `RunReport`.
- **The front end does not enable buttons until the `listen` subscription has completed.** Stop becomes
  enabled only upon receiving `runandlog://started`, so if a run could start before subscribing,
  **Stop could not be pressed for the whole time the command runs**. Subscription is all-or-nothing
  (`Promise.allSettled` → if any part fails, unlisten the ones that succeeded), so that a half-subscribed
  state is never created.
- **Even if subscription fails, open in degraded mode.** If `listen` is refused, e.g. for lack of a capability,
  read the document, enable the buttons, and show the reason in the status. Freezing here would turn
  the window into a box that can do nothing. In this state `started` never arrives, so **Stop is
  enabled to follow busy** (the default is no timeout, so if it could not be pressed, a long command could not be
  stopped from the window). A premature Stop is harmless — the backend just answers
  "nothing is running".
- **On every re-render, save and restore the scroll position of the cell list** (CYBERNEURA-DEV-717). `render`
  rebuilds the whole list with `replaceChildren`. The moment it has zero children the list can no longer scroll,
  and the browser clamps `scrollTop` to 0, so **every time a cell was run the reader was thrown back to the top of
  the document**. Even when results grow or shrink and the height changes, the browser clamps the assigned value
  to the possible range.
- **Scrolling live output to the end is done after inserting it into the DOM.** An element under construction
  does not belong to the document and has zero height, so writing `scrollTop` inside `renderCell` does nothing.
- **A running cell is made recognisable visually, not just by the button label** (same 717).
  Accent-coloured border + a pulsing halo (`cell-pulse`), and a spinning ring at the cell head (`spinner`).
  **With `prefers-reduced-motion: reduce`, both animations are stopped** (the colour and a static halo still
  distinguish it). Motion is the point, but some people cannot use the window because of motion.
- **Neutral colours get a slight tint of the accent hue** (same 717). Plain grey has zero saturation and,
  next to a blue accent, makes the screen look disabled. Saturation is raised by only a few points, and
  lightness keeps the original relationships (page < card, header/result in between). Keep the contrast ratio of
  `--muted` at 4.5:1 or more in both light and dark.
- **The cell head changes colour by state** (CYBERNEURA-DEV-895). Not yet run: unchanged; running:
  `--head-running` (a light accent colour); run (a cell that has a result — the same check as the Run / Re-run
  label): `--head-done` (grey with a little hue left), with a faded number and
  the command at `opacity: 0.5`. **Only the commands of run cells intentionally fall below 4.5:1** —
  low contrast is a requirement of the request; cells that are not yet run or are running keep their original contrast.
- **The front end does not use innerHTML.** Commands and their output are arbitrary text, and
  injecting them as HTML would let a document run scripts inside the window.
  Build everything with `textContent` and DOM creation only.
- **Without a capability, core plugin commands are refused** (`capabilities/default.json`).
  Our own commands registered with `invoke_handler` pass without a capability, so **only `event.listen`
  fails silently**. This went unnoticed until the app was opened on a real machine. Only `core:event:default` is granted;
  window / webview / menu / path are not used.
- The window is defined in `tauri.conf.json`, and the front end lives in `crates/runandlog-cli/ui/`
  (plain HTML / CSS / JS. There is no node build step, so `withGlobalTauri` is enabled).
- **Per-cell Copy button** (CYBERNEURA-DEV-735). Copies the command to the clipboard, and
  the label changes to a check mark for 3 seconds.
  - **The copied state is kept in a module variable (`copied`).** If kept on the button, the check would
    silently disappear on the re-render that comes with every write-back (`render` rebuilds every cell).
  - **The label is rewritten directly in the DOM, without re-rendering.** Re-rendering would discard
    the live output of another cell if one was running.
  - **Copy is not disabled while busy** (unlike Run). It only reads the document and changes nothing.
  - **Only `navigator.clipboard` is used for the clipboard.** We do not add the clipboard plugin
    (a Rust dependency + capability) for a single button. Both webviews serve from a
    secure origin, so it exists, but if it is refused, the error is shown in the status rather than swallowed
    (so it never becomes a button that does nothing when pressed).
  - **`writeText` is called synchronously from the click** (without crossing an await). WebKit
    (macOS's WKWebView / WebKitGTK = both targets of this app) allows clipboard writes only while the press is
    still alive as a user activation.
    **If presses are serialised so each call happens after the previous write completes, the activation expires
    while waiting and the call fails with `NotAllowedError`** (especially when a permission prompt is open).
    It was once written that way and Codex pointed it out. A press that does nothing is worse than
    being unsure which of two simultaneous presses gets the check mark.
  - **When two presses are in flight at once, "the one that responded last" wins.** What remains on the clipboard
    is whatever the platform wrote last, and promise resolution is the only clue to that.
    Do not decide by press order (press order and write order do not match).
  - **`copyGeneration` is not for counting presses.** It advances only on reload, and is used to
    discard presses awaiting a response. `forgetCopied` **always advances it, even when no check mark is
    showing** — a press awaiting a response has shown nothing on screen, so if it were not advanced, it would
    survive the reload and put a check mark on a cell that has been replaced.
  - **`reload` calls `forgetCopied` twice**: before the invoke, and right before rendering once the response
    returns. Copy can be pressed during reload, so **a press made while awaiting the response carries a generation
    later than the first call**. Without the second call, that press would grab the cell after the replacement.
  - **The glyph is U+2750 (`❐`).** More copy-like glyphs such as U+29C9 (`⧉`) and U+2398 (`⎘`) are missing from the
    fallback fonts of the Linux webview and render as tofu (measured in remote Chrome).
  - The button has a `min-width`. `✓ Copied` is about 16px wider than `❐ Copy`, and
    on a right-aligned button the left edge visibly jumps the moment it is pressed.
- **Find (Cmd+F / Ctrl+F on Linux and Windows)** (CYBERNEURA-DEV-947). WKWebView has no in-page find,
  so we have our own find bar. It targets each cell's `pre.command` and `pre.result` (`pre.live` while running).
  - **Highlights are painted with the CSS Custom Highlight API (`CSS.highlights` + `::highlight()`).**
    Wrapping in `<mark>` would split the DOM for matches that span token spans, and would have to be redone every time by both
    `render`, which rebuilds all cells, and `appendLive`, which rewrites `textContent` per chunk.
    The Highlight API does not touch the DOM, so neither needs to know that find exists. Webviews without the API
    (older than macOS 14.2 / Safari 17.2) just cannot paint; counting, navigation, and scrolling still work.
  - **Re-searching is batched into the next frame by `scheduleFind`.** After `render` the whole document, after `appendLive`
    only that block (`findAgainIn`, which swaps the matches). Live output arrives in 20ms steps, so
    the whole document is not scanned per chunk. Re-searching does not scroll (so that the place being read is not
    dragged along by the output). The current match is looked up again by the key (cell, command/result, start offset).
    **Offsets in live output are positions within "everything output in this run"** (adding `liveDropped`),
    because offsets within `live` shift every time it is truncated to the last 20000 characters.
    **When a run finishes and the live output is replaced by the result, the match is looked up again by "the number of characters from the
    end of the output" rather than by offset** (`restoreCurrent`, three Codex PR review findings). The result is preceded by a summary
    line and a fence (a cell whose output went to a file becomes just a link), so offsets do not correspond, but the output
    sitting at the end (before the closing fence `outputEnd`) is the same as in the live view. The live side does not count the trailing
    newline (`render::normalize` drops it). The amount truncated is written onto the block element itself
    (`data-dropped`, `liveBase`) at the same time as the text, because `liveDropped` is reset before the re-render at the end,
    and the value at the time the match was found becomes stale with the next output (both raised in Codex PR review). **Do not count by number of matches** — the list is cut off at 5000, and
    the result adds matches in the summary line and fences.
  - **Matches are searched with a regular expression (`giu`).** Comparing lower-cased copies shifts offsets with characters
    whose length changes, such as `İ`.
  - Matches are cut off at `FIND_MAX_MATCHES` (5000), with `+` appended to the count. The webview keeps Ranges
    tracking DOM changes, so a one-character search must not create hundreds of thousands of them.
  - **The Enter that commits an IME conversion does not advance to the next match** (`isComposing` / keyCode 229).
  - Cmd on macOS, Ctrl elsewhere. On macOS Ctrl+F moves the caret, so it is not taken over.
    Tauri's default menu has no Cmd+F / Cmd+G, so the keydown arrives as-is.
    **If Find is added to the native menu, make sure the accelerator and the DOM keydown do not both
    fire** (it has not been added). Find in the TUI is not implemented.

**The Tauri window cannot be opened here, but the front end can be verified.** `ui/` is plain HTML / CSS / JS,
so serve it over HTTP together with a page that stubs `window.__TAURI__` in the shape `{core:{invoke}, event:{listen}}`,
and have remote Chrome (agent-browser) load it to measure DOM, CSS, scrolling, and
timers. `agent-browser set media dark` switches between light and dark too.
**Bind the server with `--bind 127.0.0.1` or a LAN address, and always stop it when verification is done.**
Rust-side verification (build and `cargo test`) is still needed separately. Only the check of opening it on a real machine (macOS)
must be done in an environment where it can actually run.

## Showing the prose around cells (CYBERNEURA-DEV-1047)

Headings and paragraphs are shown in the TUI / GUI / `--list`. **Parsing is done only by the core's `parse::read_prose`**;
the GUI just receives `CellView::heading` / `context` / `notes` and `DocumentView::epilogue` and
assembles them (in line with the policy of not adding GUI-specific parsing).

- **The span is "from right after the previous cell (including its result block and `Result:` paragraph) up to this cell's fence".**
  The last heading in that span becomes `heading` (shown at the cell head), what precedes it becomes `context` (shown outside the cell, above it),
  and the paragraphs after it become `notes` (shown inside the cell, above the command). If the span has no heading, everything is `notes`.
  Everything after the last cell is `Document::epilogue`.
- **A heading is only "one in the immediately preceding span".** Even if a second cell sits under the same heading,
  the second cell does not show the heading (in the document, too, there is no heading right before that cell).
- Skipped: code fences (including fences inside result blocks), HTML comments (including markers; ones starting or ending mid-line are also removed by `strip_comments`, raised by Codex CLI. However, a `<!--` inside a code span or escaped is not treated as the start of a comment (`comment_open`, raised in Codex PR review — merely quoting the delimiter in prose would otherwise make everything after it disappear. Code spans spanning multiple lines are not tracked)),
  thematic breaks, and a leading YAML front matter. Headings are ATX and setext. Inline Markdown is
  shown as-is without rendering (the GUI uses `textContent`).
- **Pipe tables are split out of paragraphs by the core's `parse::split_tables`** (CYBERNEURA-DEV-1378).
  `read_prose` leaves a table inside the paragraph it sits in (tables are often written with no blank line
  above them), and `split_tables` is a second pass over a paragraph: a line with `|` followed by a delimiter
  row of the same width starts a table, and every following line with `|` is a row (padded / cut to the
  header's width, as GitHub does). A delimiter row needs a `|`, so a lone `---` stays a setext heading / rule.
  Raised by Codex CLI and handled: a quote / list item line ends the table (`starts_block`); `---` right
  under a table is a rule, not a setext underline that would turn the table into a heading (`ends_in_table`
  in `read_prose`); a delimiter cell takes at most one colon a side; a table over `MAX_TABLE_CELLS` (20,000)
  cells once padded stays text; the GUI keeps each table's sideways scroll across re-renders
  (`restoreTableScrolls`, by document order).
  The GUI's `ProseView` carries the result as `table` (`TableView`; paragraphs have no `table` key), and
  `notes` became `Vec<ProseView>` for the same reason. The front end only draws it (`appendParagraph` /
  `buildTable`, `textContent` per cell, inline Markdown in cells as written). The TUI and `--list` show the
  lines as written.
- It is not included in the GUI's find (Cmd+F) targets (find assumes its keys are `pre` elements inside `section.cell`).
- The TUI does not wrap prose (it is cut off at the screen width, like commands and results).
- **In the TUI, PageUp / PageDown can scroll away from the selected cell** (`App::follow_selection`,
  raised in Codex PR review). Previously `adjust_scroll` pulled back to the top of the selected cell every time,
  so the final prose belonging to no cell (`epilogue`) could not be reached. Pressing any other key resumes following.

## App icon

`crates/runandlog-cli/icons/icon.png` (`bundle.icon` in `tauri.conf.json`).
**Do not draw it by hand; generate the fixed source image with `scripts/generate-icon.py`** (it runs on the standard library only;
this repository has no image toolchain, and adding one for a single asset is not worth it).
To change it, fix the script, regenerate the source image, and propagate it to the per-size images with `bash scripts/generate-macos-icons.sh`.

The dimensions of the widely used macOS icon template (Big Sur and later) are encoded as constants in the
script (CYBERNEURA-DEV-519. **The previous icon was a 64x64 single-colour square with no rounded corners
and no margin**, and looked bigger and squarer than other icons in the Dock):

- On a 1024x1024 canvas, **the body is 824x824, centred**. A transparent margin of 100px (about 10%)
  remains on all four sides. Icons following the template look the same size because of this margin,
  and **drawing to fill the canvas makes it render larger than the neighbouring icons** — that is
  what made it look "square and odd". It is also where shadows and badges go.
- **The corner radius is 22.5% of the body** (185.4px). Apple's real shape is a continuous-curvature squircle rather than circular arcs,
  but circular arcs of this radius are the template's grid, and at the sizes actually seen they are indistinguishable.
- Everything other than those three (canvas / body / corner radius) — gradient, highlight, play mark — is
  this app's design and may be changed freely.

After generating, the reliable check is **to count the alpha extent and verify the dimensions**
(whether the body spans 100..923, i.e. 824x824). Running headless, the look in the Dock
cannot be checked, but scaling down and compositing over a dark background shows whether small sizes get crushed.

## CI (GitHub Actions)

`.github/workflows/test.yml` is **both the PR check and the gatekeeper for releases**.
It has both `workflow_call` and `pull_request`, so it is a single workflow.

- **`release.yml` calls it via `uses:` and makes `build` `needs: test`.**
  The jobs of a called reusable workflow **join the same run as the caller**, so `needs` works.
  As a separate workflow triggered by the same push, it could not be depended on via `needs`, and the release
  would proceed normally even with red tests (policy: `docs/build-requires-tests.md` in cyberneura-github-actions).
- **The matrix matches `release.yml`'s build matrix down to the features.** macOS uses the default features
  (including the GUI), and Linux uses `--no-default-features`. The other way round would "test code that is not shipped
  and not test code that is shipped". GUI tests run only on the macOS leg.
- **The `test` job is conditioned on `needs: plan` + release only.** Tests do not run on pushes to main
  that do not change the version, but that gap is filled by the PR-side trigger.
- `cargo fmt --check` depends on neither OS nor features, so it runs only on the ubuntu leg.

## Releases

**Changing the version in `Cargo.toml` on main makes that a release.** The workflow
is triggered by **every push to main** and by `workflow_dispatch`, and decides whether to run solely by **whether that version has already
been released**. `scripts/release.sh [patch|minor|major]` is a thin script that just
rewrites the version and pushes; editing Cargo.toml by hand and pushing does the same.

- **No paths filter.** Restricting to `Cargo.toml` would be cheaper, but a release that failed because of a bug in the build, the workflow, or
  the code could no longer be retried by "fix the cause and push" (that fix does not
  touch Cargo.toml). The check costs one checkout and one API call, cheaper than what filtering would save.
- **The check is "has that version been published", not "did the diff change the version".** Squash /
  rebase / direct push change the shape of the diff, but not whether it has been published. With this check,
  **running it any number of times is safe**, and a failed release resumes as soon as the cause is fixed and pushed
  (no need to bump the version again).
- **The published check reads only 404 as "not published".** Reading a rate limit or an outage as not published would
  build and publish an already-published version again.
- **mac is arm64 only, default features (including the GUI), Developer ID signed + notarized.** The cask
  adds quarantine, so without signing and notarization the recipient's first launch is blocked. A bare binary
  cannot be stapled, so the ticket stays on Apple's side (Gatekeeper fetches it online). **Check the notarization result
  via `status` in `--output-format json`** — `notarytool` can exit successfully as long as the
  submission completed.
  - **Signing is done in a `sign` job separate from build.** In the build job, the build scripts of dependency crates
    run with the job's permissions, and by writing to `$GITHUB_PATH` they could replace `openssl` / `security` for later
    steps (the steps that hold the certificate and password). `sign` receives the bare binary as an artifact on a fresh runner,
    signs and notarizes it without running cargo at all, and deletes the keychain right away. Packaging is done by the
    `release` job, which checks out the repository.
- **Linux uses `--no-default-features`.** The default features include Tauri, and a binary dynamically
  linked against webkit2gtk will not start in environments without the same webkit. It is still dynamically linked
  against glibc, though, so it does not "run on any Linux".
- **The Release is created as a draft, and published after counting the assets.** `gh release create` creates the
  Release first and uploads afterwards, so a failure midway leaves a published Release with missing contents.
  **A leftover draft is deleted and recreated by the next run** (published ones are not touched). Drafts are found
  by listing `/releases` — **`releases/tags/{tag}` does not return drafts** (a draft has no tag
  yet), so looking up by tag would create two drafts with the same name.
- **The Homebrew cask is not touched in this repository.** A workflow in `cyberneura/homebrew-tap`
  checks each project's latest Release hourly and updates the cask. Having each project push to the tap
  would mean **handing a token that can write to the tap to every project**.
- **crates.io publishing is enabled by a repository variable** (`gh variable set PUBLISH_CRATES --body true`).
  Secrets cannot be referenced in `if:`, so the branch is on a variable. `CARGO_REGISTRY_TOKEN` is required.
- **`runandlog` is published with `--no-verify`.** Verification means unpacking the package and building it,
  but during that `tauri-build` writes `gen/schemas/` into the source directory, so cargo
  refuses with "build script modified files outside OUT_DIR" (the v0.3.0 publish actually failed
  this way). There is nowhere to restore verification to, so it is substituted by the fact that **the build job of the same run**
  builds the same commit with the same features on the same OS. `runandlog-core` is verified as before.
- **crates.io is queried through the sparse index (`index.crates.io`).** cargo resolves through the index, and
  even if a crate shows up on the web API, the build of the next crate fails if the index has not caught up. Already-published
  versions are skipped, so a re-run after only one of them succeeded continues from where it stopped.
- **Whether to publish to crates.io is decided separately from the GitHub Release** (in the plan job,
  `scripts/crate-indexed.sh` asks the index). Even if only the publish fails after the Release is out,
  the next push or a `workflow_dispatch` runs just the crates job. By then main may have moved on,
  so **the crates job checks out the release tag into `release/`** and confirms that the tag's version matches the
  decided version before publishing. The check script used is the one from the workflow's
  commit (tags from before the script was introduced do not have it).
  - It runs even when `release` is skipped only in the case "it was already released at plan time". `release` is also
    skipped on test or build failures, so allowing it on `skipped` alone would publish an unreleased
    version to crates.io only.

## Live display of output during a run

`exec::run_streaming` (CYBERNEURA-DEV-567). It is just `run_cancellable` plus "a callback invoked
whenever output arrives"; `run` / `run_cancellable` delegate to it.

- **The callback is invoked on a dedicated thread (`report_output`).** Invoking it from the reader thread would
  stall pipe reads for as long as rendering lags, so **the pipe fills up and the command stalls**. Invoking it
  from the polling loop instead would **queue timeout and cancel detection behind the callback**
  (a front end printing to a pipe nobody reads actually gets stuck; pointed out in Codex review).
  It is placed in neither, so a slow front end loses only the freshness of the display.
  The regression test is `a_front_end_that_blocks_does_not_hold_up_the_timeout`.
  - The tick is `POLL_INTERVAL` (20ms). Termination wakes it immediately by dropping the sender of the done channel
    (so the polling wait is not paid on every run).
  - **Only the final call is made on the caller's thread after join** (also stated in the public docs.
    It is never called from two threads at once, but **it is not always the same thread**, so
    keep the callback's state in the closure — do not put it in a thread-local). The remaining output and the final `output` are built
    from one snapshot under the same lock (if interrupted, the two would disagree), but **the callback is invoked after
    releasing the lock**. When DRAIN_GRACE is exceeded the reader thread is still running, and invoking a slow callback while
    holding the lock would block the reader on the mutex = the pipe would no longer be drained.
  - The callback is passed to a thread, so it needs `+ Send`. Panics are returned to the caller with
    `resume_unwind` (not swallowed on an invisible thread).
- **Concatenating the chunks exactly equals `ExecOutcome::output`.** Front ends can write "show progress →
  show the result at the end" without reconciling anything. For this invariant:
  - `decode_ready` **holds back characters split at a read boundary** (`Utf8Error::error_len() == None`).
    Turning them into U+FFFD on the spot would make replacement characters appear only in the progress, not in the final output (`from_utf8_lossy`).
  - Conversely, **bytes that can never form a character are replaced on the spot**. Waiting for more would freeze the display for the whole run.
  - At the end of the loop, the unreported bytes (including a held-back fragment) are flushed once, lossily.
- How each front end uses it:
  - **Non-interactive runs** (`main::print_as_it_arrives`) write as soon as output arrives and **flush explicitly
    every time**. stdout is line-buffered on a terminal and block-buffered when redirected, so
    progress lines without a newline — the main thing live display is for — would stay in the buffer.
    **Do not use `print!`.** It is a macro that panics when stdout is closed (`| head` etc.), and
    this callback is invoked from inside the run, so the panic would escape **before the results are written back
    to the Markdown**, wasting the command run (raised in Codex PR review).
    Write with `write_all`, and if that fails, silently discard the rest of that run
    (test: `printing_gives_up_quietly_once_the_output_has_nowhere_to_go`).
  - **TUI**: the worker sends chunks over mpsc, and the render thread picks them up with `try_recv`
    (`collect_output`). With a shared lock the worker could end up waiting on rendering. The display is the
    last 3 lines of `live::LiveOutput`, and **the number of lines is always fixed**. If the height changed with every
    output, the cells below would move and be unreadable.
  - **GUI**: the `runandlog://output` event (`{index, text}`). The front end does not re-render but updates
    `textContent` of `pre.live` directly (so as not to rebuild every cell many times per second).
    It always checks `index` and **discards chunks for any cell other than the running one** (preventing the last
    chunk of the previous run from appearing under the next cell). On receiving `runandlog://document`,
    the live display is discarded — the result has been written back by then, so keeping it would hide the new result.
- **Live display is not a record, so every layer has a cap**: `LiveOutput` is 16KiB,
  one GUI chunk is 64KiB, and the front end keeps 20000 characters. The full output is written back to the Markdown when
  the run ends, so nothing is lost by discarding. **When trimming the tail, do not split characters**
  (in Rust `live::tail` respects char boundaries; in JS a low surrogate advances by one).
- **When `max_output_bytes` (default 8MiB) is reached, the live display stops too.** This is because the reader discards everything
  past that point — the same reason, and the same point, at which `output` stops growing. In Codex PR review it was
  suggested that "the live display alone should keep streaming the tail", but **this is not adopted**: it would break the
  "concatenation of chunks = final output" invariant above and display text that does not end up in the result.
  Past 8MiB the result itself is truncated (`truncated`).

## Cell colours (TUI)

"Cells whose run finished in this session" are distinguished by colour (CYBERNEURA-DEV-567).
Waiting = green / running = yellow / done = blue / failed = red, and done and failed cells also have their command line dimmed.

- **Having results left in the file does not mean "run".** If every cell showed the done colour when opening a document
  that was run yesterday, the crucial information — how far `a` (run all) has progressed — would be unreadable.
  The check uses only `App::finished` (index of a cell run in this session → success/failure).
- **`finished` is discarded on reload.** After reloading, an index does not necessarily point to the same cell.

## Command syntax highlighting (CYBERNEURA-DEV-889)

`runandlog_core::highlight` splits a command into tokens (kind + byte range), and both the TUI and the GUI
colour it from those.

- **There is exactly one lexer, in the core.** The GUI receives `CellView::tokens` (kind name + text) and
  just builds `tok-<kind>` spans; the JS side has no knowledge of the shell. Same reason as the policy of
  not adding GUI-specific parsing (the GUI section above).
- **It is a lexer, not a parser, and it never fails.** The input is Markdown in the middle of being edited, so including unclosed
  quotes, `${`, and here-documents, **the tokens always cover the whole text with no gaps**. A test
  checks this invariant with pseudo-random input (`any_text_is_covered_without_panicking`).
  When the concatenation does not match the command, the front end falls back to plain text (losing colour, but never the text).
- Dependencies (syntect / tree-sitter etc.) were not added because the accuracy is not worth enlarging the distributed binary and
  THIRD-PARTY-NOTICES. Colouring only needs to pick out "which word is the program name and
  where a string ends".
- **Determining command position**: start of line; after `|` `&&` `;` `(` etc.; after reserved words such as `then` `do`;
  after `$(` `<(` `>(` and an opening backquote; after `NAME=value`. **The word that is a redirect target does not
  consume the command position** (in `> out.txt echo hi`, `echo` is the command; raised by Codex CLI).
  **Substitutions and subshells are tracked open/close with the `nesting` stack, and on close the state from before the open is restored**
  (in `A=$(date) env`, `env` is the command; in `echo $(date) x`, `x` is an argument; raised by Codex CLI).
  The body of a here-document is treated as a string (so that `rm -rf` in the body is not coloured as a command).
  **`case` tracks "pattern position or branch body" with the `cases` stack** (after `in` and `;;` is a
  pattern, after `)` is the body; raised in Codex PR review). Without tracking, commands in the body would be uncoloured and
  the next pattern would be coloured as a command. **So that the `)` of a substitution inside a pattern (`$(printf x))`) does not end
  the pattern, `Case::depth` holds the `nesting` depth at the time the case was opened.**
  In `time -p cmd`, `-p` is an option of `time` and does not consume the command position (`after_time`).
  **`Nest` also saves and restores the progress of the enclosing `for` / `case` (`stage` / `case_pending`)**
  (so that the `in` of `case $(printf y) in` is reached). **Inside arithmetic (`$((` / `((`), nothing — including parentheses and `;` —
  becomes a command position** (`Nest::arithmetic`; so that `total` in `$(( (total + 1) * 2 ))` and
  `i` in `for ((i = 0; i < n; i++))` are not coloured as commands. Newlines inside arithmetic likewise). `<(` inside a case pattern
  also opens a nest. All raised in Codex PR review.
  **`Nest::expression` is the umbrella for "contexts with no commands inside": arithmetic, `[[ ... ]]`, and array values
  `name=(...)`**. Inside, none of `(` `&&` `;` or newline becomes a command position
  (so that `-f` in `[[ -f a && -f b ]]` and `one` in `items=(one two)` are not coloured as commands).
- Codex PR review kept producing two shell-grammar edge cases every round (6 rounds).
  **The lexer does not try to follow the full bash grammar** — the goal is readability, and a mistake costs one colour.
  Decide on new findings by "is this a commonly written form".
- **Known limitation: everything inside double quotes is string-coloured** (`date` in `"$(date)"` is not coloured as
  a command either). It is not a wrong colour, just "no colour", so it was deferred (Codex PR review, 7th finding).
  Doing it would require rebuilding `double_quoted_len` and `word_len` to track nesting of `$(` / backquotes
  and inner quotes (`"$(echo ")")"`) inside quotes, which easily breaks the invariant that tokens cover
  the whole text. If you do it, `any_text_is_covered_without_panicking` must pass.
  **What follows a substitution closed mid-word continues the same word** (`word_continues`; treating `/bin` in
  `PATH=$(pwd)/bin env` as a new word would eat the command position and leave `env` uncoloured).
  **Function definitions (`NAME()` / `function NAME`) colour the name as a variable and make the body a command position**
  (reading `(` as a subshell would leave everything from `{` uncoloured). Both raised in Codex PR review.
- **The TUI does not colour done or failed cells** (a single dim colour, as before). Restoring colour would
  defeat the dimming that shows the progress of run all.
- GUI colours are at least 4.5:1 against `--card` in both light and dark (CSS variables `--tok-*`).

## License notices (CYBERNEURA-DEV-880)

`LICENSE` (MIT, Cyberneura) and `THIRD-PARTY-NOTICES.txt`, which lists the licenses of the crates included in the
distributed binary, are placed at the repository root. Both are embedded in the binary and shown by
`runandlog --license` and by **Third-Party Licenses** in the GUI menu (on macOS directly below About in the app menu,
elsewhere directly below About in Help).

- **When adding or upgrading dependencies, re-run `scripts/generate-third-party-notices.sh` and
  commit the result.** `tests/third_party_notices.rs` checks "are the direct dependencies listed with their Cargo.lock
  versions" and "are all listed crates present in Cargo.lock", so forgetting this makes
  `cargo test` fail. Dependabot PRs do not update the notices either, so regenerate on the same
  branch before merging. Requires cargo-about (`cargo install cargo-about --locked --features cli`) and
  python3. Generation takes a few minutes (queries to clearlydefined).
- **cargo-about is run twice and the results merged.** Releases differ down to the features between macOS (aarch64, default features = including the GUI)
  and Linux (x86_64, `--no-default-features`), and a single cargo-about run can only take
  one set of features. Running both targets in one pass with the default features would list GTK / WebKit crates that
  ship in neither distribution. If you change release.yml's build matrix, align the script too.
- **Our own crates (runandlog / runandlog-core) are excluded by the script.** They are published to crates.io,
  so `publish = false` cannot be set, and cargo-about's `[private] ignore` does not apply.
- **Do not widen `accepted` (about.toml) on your own.** If GPL / LGPL / AGPL-family licenses appear, the distribution terms change, so
  check with a human.
- **Embedding goes through symlinks inside the crate** (`crates/runandlog-cli/LICENSE` and
  `THIRD-PARTY-NOTICES.txt` are symlinks to the repository root). Only the crate's
  directory is published to crates.io, so pointing outside the package, as in `include_str!("../../../...")`,
  makes the build after `cargo package` fail. cargo packages the contents of symlinks as regular files
  (confirmed with `cargo package --list` and the contents of the .crate).
- The license window (`ui/licenses.html`) has no capability. It calls only our own
  `third_party_notices` command, which passes without a capability.
- **The FILE argument is `Option<PathBuf>` + `required_unless_present = "license"`.** clap's
  `exclusive = true` alone cannot lift the check for a required positional argument, and `runandlog --license`
  failed with "FILE is missing". `exclusive` is kept to "reject `--license` given together with
  other arguments".

## Not implemented / future work

- Colour done cells in the GUI (currently a TUI-only feature).

## Verification

```shell
cargo test                  # 210 tests (count on linux; 1 test looks at /proc,
                            #          3 tests look at DISPLAY)
cargo clippy --all-targets  # keep it at zero warnings
cargo fmt --all --check
```

To run it by hand, use `examples/run-example.sh`. It creates a throwaway copy of `examples/exam.md`
under a gitignored directory (`examples/scratch/`) and runs that, so **however many times you run it,
the working tree stays clean**. A run also writes result files next to the document, so never run the sample
directly. Arguments are passed through as-is.

```shell
examples/run-example.sh --run-all
RUNANDLOG=./target/release/runandlog examples/run-example.sh --list  # with an existing binary
```

To just create a copy, use `examples/create-test-copy.sh` (prints the path to stdout). Use it when you want to
assemble the run yourself, as in the pty launch below.

The TUI needs a terminal, so it is outside automated testing. To check it by hand, allocate a pty and launch it.

```shell
DOC=$(examples/create-test-copy.sh)
# GNU (linux). Pass the path via an environment variable and quote it inside (it breaks if the path contains spaces)
(sleep 1; printf 'q'; sleep 1) | DOC="$DOC" script -qec 'stty rows 30 cols 100; ./target/debug/runandlog "$DOC"' /dev/null
# BSD (macOS). There is no -c
(sleep 1; printf 'q'; sleep 1) | script -q /dev/null ./target/debug/runandlog "$DOC"
```

To send Ctrl-C, use `printf '\003'`. However, **under a sandbox a pty cannot be allocated
(`script: openpty: Operation not permitted`) and the TUI cannot be launched**, so in that case check by hand.

## macOS app icon

Run `bash scripts/generate-macos-icons.sh` with Xcode on macOS. Fixed artwork,
layout measurements, size-specific sources and the verification command are in
`resources/app-icons/README.md`. Small @1x backgrounds reach the canvas edges;
the glyph fits inside the central 80%. Larger backgrounds have 10% outer margins.
Do not replace the generated ICNS with `tauri icon` / `iconutil`: preserve the
ARGB small entries and the matching Assets.car / CFBundleIconName registration.
Native rendering is verified by `.github/workflows/macos-icons.yml`.

The distributed CLI is a bare executable: its GUI embeds ICNS and sets
NSApplication’s icon at startup. Bundle registration applies only to optional
.app builds; it cannot give a bare binary a System Settings bundle icon.
