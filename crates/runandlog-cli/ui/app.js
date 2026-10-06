// Window logic.
//
// The Rust side owns every piece of state that matters: it parses the Markdown,
// runs the commands, and writes the results back. This file only draws what it is
// given and sends the two actions the user can take (run a cell, reload).
//
// Everything is built with DOM calls rather than innerHTML on purpose: commands
// and command output are arbitrary text, and pasting them into HTML would let a
// document run script in the window.

const { invoke } = window.__TAURI__.core
const { listen } = window.__TAURI__.event

const pathEl = document.getElementById('path')
const cellsEl = document.getElementById('cells')
const statusEl = document.getElementById('status')
const runAllButton = document.getElementById('run-all')
const stopButton = document.getElementById('stop')
const reloadButton = document.getElementById('reload')
const passwordDialog = document.getElementById('password')
const passwordForm = document.getElementById('password-form')
const passwordPrompt = document.getElementById('password-prompt')
const passwordInput = document.getElementById('password-input')
const passwordDecline = document.getElementById('password-decline')
const findBar = document.getElementById('find')
const findInput = document.getElementById('find-input')
const findCount = document.getElementById('find-count')
const findPrevButton = document.getElementById('find-prev')
const findNextButton = document.getElementById('find-next')
const findCloseButton = document.getElementById('find-close')

/** Index of the cell being run, or null when idle. */
let running = null
/**
 * What the running command has printed so far.
 *
 * Only ever the tail of it: the whole output goes into the Markdown when the
 * command finishes, so keeping more here would grow the window's memory with a
 * command that prints without stopping, for text nobody is reading.
 */
let live = ''
/**
 * How much has been cut off the front of `live` during this run. Lets find name
 * a match in the live output by where it is in everything the command printed,
 * which does not move when older text is dropped; its offset in `live` does.
 */
let liveDropped = 0
/** How much of a running command's output the window keeps. */
const LIVE_MAX_CHARS = 20000
/** Buttons are disabled while a run is in flight. */
let busy = false
/**
 * Index of the cell whose Copy button is showing its check, or null.
 *
 * Held here rather than on the button so that a redraw -- one arrives whenever a
 * run is written back -- keeps showing the check on the cell that was copied,
 * instead of silently putting the label back.
 */
let copied = null
/** Timer that takes the check away again, so a second copy can restart it. */
let copiedTimer = null
/**
 * Raised whenever copies still waiting for an answer must be thrown away.
 *
 * Only reload does that, and only because the check is held by index: a press
 * whose `writeText` has not answered yet has nothing on screen to clear, so
 * without this it would come back after the redraw and mark whichever cell had
 * landed on its index.
 *
 * It deliberately does **not** count presses. Two presses in flight at once are
 * settled by which write the platform finished last, and a promise resolving is
 * the only sign of that; so the last answer wins, rather than the last press.
 */
let copyGeneration = 0
/** How long the Copy button shows its check. */
const COPIED_MS = 3000
/**
 * Whether the backend's events are being listened to. See `start`.
 *
 * Taken for granted until `start` learns otherwise, so that the moment before the
 * subscriptions settle -- where nothing is running yet -- does not offer Stop.
 */
let subscribed = true

/**
 * Id of the password request the dialog is answering, or null when it is closed.
 *
 * The backend numbers requests so that an answer typed after the command has
 * ended -- and a new one has asked -- cannot go to the wrong one.
 */
let passwordId = null

/** Shows a command's request for a password. */
function askPassword({ id, prompt }) {
  passwordId = id
  passwordPrompt.textContent =
    running === null ? prompt : `Cell ${running + 1} asks: ${prompt}`
  passwordInput.value = ''
  if (!passwordDialog.open) {
    passwordDialog.showModal()
  }
  passwordInput.focus()
}

/**
 * Sends the answer, or null to decline, and closes the dialog.
 *
 * The field is emptied before anything is awaited, so the password does not sit
 * in the page any longer than it takes to hand it over.
 */
async function answerPassword(answer) {
  if (passwordId === null) {
    return
  }
  const id = passwordId
  dismissPassword()
  try {
    await invoke('answer_password', { id, answer })
  } catch (error) {
    setStatus(String(error), 'error')
  }
}

/**
 * Closes the dialog without answering. For when the command that asked has
 * ended: the backend has already declined on its behalf.
 */
function dismissPassword() {
  passwordId = null
  passwordInput.value = ''
  if (passwordDialog.open) {
    passwordDialog.close()
  }
}

function setStatus(text, kind) {
  statusEl.textContent = text
  statusEl.dataset.kind = kind || 'info'
}

function setBusy(value) {
  busy = value
  runAllButton.disabled = value
  reloadButton.disabled = value
  // Stop is the one button that only makes sense while a command is running. With
  // the events subscribed it waits for the backend to say one has started, since a
  // press that beats the `run_cell` / `run_all` call there finds nothing to stop
  // and is dropped; it then stays on between the cells of a batch, where the
  // backend remembers the request. Without them that signal never comes, so being
  // busy has to be enough -- otherwise a command with no timeout could not be
  // stopped from the window at all. A press too early is harmless: the backend
  // reports there was nothing running and clears the request when one starts.
  stopButton.disabled = value ? subscribed : true
  for (const button of cellsEl.querySelectorAll('button.run')) {
    button.disabled = value
  }
}

/**
 * Draws the document. Called for the first paint and after every write-back.
 *
 * Every draw replaces the whole list, which drops the scroll position: with no
 * children the list has nothing to scroll, so the browser clamps `scrollTop` to
 * zero and running a cell threw the reader back to the top of the file. The
 * position is taken before the swap and put back after it, once the new cells
 * give the list its height again.
 */
function render(doc) {
  const scrollTop = cellsEl.scrollTop
  pathEl.textContent = doc.path
  pathEl.title = doc.path
  cellsEl.replaceChildren()

  if (doc.cells.length === 0) {
    const empty = document.createElement('p')
    empty.className = 'empty'
    empty.textContent = 'No shell / sh / bash / zsh code block found.'
    cellsEl.append(empty)
    scheduleFind(null)
    return
  }

  for (const cell of doc.cells) {
    cellsEl.append(renderCell(cell))
  }
  // A result appearing or disappearing changes the height, so the old offset can
  // be past the new end. Assigning it is enough: the browser clamps it to what
  // the list can now scroll, which keeps the top of the view as close to where
  // it was as the new content allows.
  cellsEl.scrollTop = scrollTop
  // The live block is scrolled to its end here rather than while it is being
  // built: a detached element has no height, so the scroll would have gone
  // nowhere.
  scrollToEnd(liveElement(running))
  // Every cell is new, so the matches found in the old ones point at text that is
  // no longer on screen.
  scheduleFind(null)
}

function renderCell(cell) {
  const section = document.createElement('section')
  section.className = 'cell'
  section.dataset.index = String(cell.index)
  // A cell holding a result has been run, whether in this window or before the
  // file was opened: the result is in the document either way, and it is the same
  // thing the Run button goes by when it says Re-run.
  const hasResult = cell.result !== null && cell.result !== undefined
  if (running === cell.index) {
    section.classList.add('running')
  } else if (hasResult) {
    section.classList.add('done')
  }

  const head = document.createElement('div')
  head.className = 'cell-head'

  const number = document.createElement('span')
  number.className = 'number'
  number.textContent = String(cell.number)
  head.append(number)

  if (running === cell.index) {
    // A turning ring beside the number, so that a cell that is working says so
    // even where the head is the only part on screen. Marked as decoration: the
    // button label beside it already says "Running…" in words.
    const spinner = document.createElement('span')
    spinner.className = 'spinner'
    spinner.setAttribute('aria-hidden', 'true')
    head.append(spinner)
  }

  const button = document.createElement('button')
  button.className = 'run'
  button.type = 'button'
  // The label is a glyph plus a word, so the button reads the same way whether or
  // not the glyph renders. A cell holding a result says Re-run, since pressing it
  // replaces that result rather than adding one.
  button.textContent =
    running === cell.index ? '▶ Running…' : hasResult ? '↻ Re-run' : '▶ Run'
  button.disabled = busy
  button.addEventListener('click', () => runCell(cell.index))
  head.append(button)

  if (cell.out_file) {
    const out = document.createElement('span')
    out.className = 'out'
    out.textContent = `→ ${cell.out_file}`
    head.append(out)
  }

  // Copy sits at the far end of the head, away from Run: the two do very
  // different things, and one of them runs a command the reader may only have
  // wanted the text of. Appended last so that it stays at the end whether or not
  // the cell writes to a file; the stylesheet is what pushes it over.
  //
  // Not disabled while a run is in flight, unlike Run: copying reads the document
  // and changes nothing, so there is no reason to make the reader wait for a
  // command to finish.
  const copy = document.createElement('button')
  copy.className = 'copy'
  copy.type = 'button'
  copy.title = 'Copy the command'
  copy.addEventListener('click', () => copyCommand(cell.index, cell.command))
  dressCopyButton(copy, cell.index)
  head.append(copy)

  const command = document.createElement('pre')
  command.className = 'command'
  fillCommand(command, cell)

  section.append(head, command)

  if (running === cell.index) {
    // While a cell runs, its live output takes the place of the result of the
    // previous run. Showing both would stack one cell's output from two different
    // runs, which reads as a single long result.
    const output = document.createElement('pre')
    output.className = 'result live'
    output.textContent = live
    section.append(output)
  } else if (hasResult) {
    const result = document.createElement('pre')
    result.className = 'result'
    result.textContent = cell.result
    section.append(result)
  }
  return section
}

/**
 * Gives a Copy button the label and the mark for the state it is in.
 *
 * The label is a glyph plus a word, like Run: the button reads the same way
 * whether or not the glyph renders. The `copied` attribute is what the stylesheet
 * colours, so the check is told from the default label by more than its shape.
 *
 * The glyph is U+2750, from the same dingbat block as the check, rather than one
 * of the copy-shaped characters higher up (U+29C9, U+2398): those are missing
 * from the fonts a plain Linux webview falls back to, and a tofu box beside a
 * word is worse than a plainer pair of squares.
 */
function dressCopyButton(button, index) {
  const isCopied = copied === index
  button.textContent = isCopied ? '✓ Copied' : '❐ Copy'
  button.dataset.copied = String(isCopied)
}

/**
 * Writes a cell's command into `pre`, one span per coloured piece.
 *
 * The backend does the lexing (the TUI colours from the same pieces); this only
 * maps each kind onto a class. Built from text nodes like everything else here.
 * Falls back to the bare text if the pieces do not add up to the command, so a
 * mismatch can cost the colours but never the command itself.
 */
function fillCommand(pre, cell) {
  const tokens = Array.isArray(cell.tokens) ? cell.tokens : []
  if (tokens.map((t) => t.text).join('') !== cell.command) {
    pre.textContent = cell.command
    return
  }
  for (const token of tokens) {
    if (token.kind === 'plain') {
      pre.append(token.text)
    } else {
      const span = document.createElement('span')
      span.className = `tok-${token.kind}`
      span.textContent = token.text
      pre.append(span)
    }
  }
}

/** Puts the command on the clipboard and shows the check on that cell. */
async function copyCommand(index, command) {
  // `navigator.clipboard` is the only way out of here: the window has no clipboard
  // plugin, and adding one for a button would mean a Rust dependency and a
  // capability for what the webview already does. Both webviews serve the window
  // from a secure origin, so it is there -- but a refusal (a webview without it, a
  // user declining) has to be said out loud rather than look like a button that
  // does nothing, which is what the status line is for.
  //
  // **Called straight from the click, before anything is awaited.** WebKit -- both
  // targets here -- only allows a clipboard write while the press that asked for
  // it is still counted as user activation. Holding the call back to run it after
  // an earlier write settled (to make two presses land in the order they were
  // made) spends that activation waiting, and the second press then fails outright
  // with NotAllowedError. A press that does nothing is a worse outcome than the
  // one it would buy: the check briefly naming the other of two commands pressed
  // within the same moment (Codex review).
  const generation = copyGeneration
  try {
    await navigator.clipboard.writeText(command)
  } catch (error) {
    // Reported unless a reload has since thrown this press away: after one, the
    // status belongs to the reload, and a prompt declined afterwards would
    // replace it with an error about a press the reader has moved on from.
    if (generation === copyGeneration) {
      setStatus(`Could not copy: ${error}`, 'error')
    }
    return
  }
  // Reloaded while this was in flight: the cells on screen are not the ones this
  // press was made against, so its index means nothing now.
  if (generation !== copyGeneration) {
    return
  }
  showCopied(index)
  setStatus(`Copied the command of cell ${index + 1}.`, 'ok')
}

/**
 * Shows the check on one cell's Copy button for `COPIED_MS`.
 *
 * The label is written straight into the button rather than by redrawing: a redraw
 * replaces every cell, which would throw away the live output of a command that is
 * running while the reader copies a different cell.
 */
function showCopied(index) {
  clearTimeout(copiedTimer)
  const previous = copied
  copied = index
  // A press on a second cell while the first still shows its check: only one cell
  // was copied, so the first one goes back at once instead of keeping a check that
  // is no longer true.
  if (previous !== null && previous !== index) {
    repaintCopyButton(previous)
  }
  repaintCopyButton(index)
  copiedTimer = setTimeout(() => {
    copied = null
    copiedTimer = null
    repaintCopyButton(index)
  }, COPIED_MS)
}

/**
 * Drops the check: the state, what is on screen, and any copy still in flight.
 *
 * The generation is raised first and unconditionally. A press whose `writeText`
 * has not answered yet -- a permission prompt is open, say -- leaves nothing on
 * screen to clear, and left valid it would put its check on whichever cell ends
 * up at its index once the reload has redrawn the list.
 */
function forgetCopied() {
  copyGeneration += 1
  if (copied === null) {
    return
  }
  clearTimeout(copiedTimer)
  copiedTimer = null
  const was = copied
  copied = null
  repaintCopyButton(was)
}

/** Re-dresses a cell's Copy button, if that cell is on screen. */
function repaintCopyButton(index) {
  const button = cellsEl.querySelector(
    `section.cell[data-index="${index}"] button.copy`,
  )
  if (button) {
    dressCopyButton(button, index)
  }
}

/** Keeps the newest line in view, the way a terminal does. */
function scrollToEnd(element) {
  if (!element) {
    return
  }
  element.scrollTop = element.scrollHeight
}

/** Adds a piece of output to the live view of the cell being run. */
function appendLive(index, text) {
  // A piece can arrive after the window has moved on -- the tail of a run whose
  // result is already drawn. Showing it under the cell running now would put one
  // command's output beneath another's.
  if (running !== index) {
    return
  }
  live += text
  if (live.length > LIVE_MAX_CHARS) {
    let start = live.length - LIVE_MAX_CHARS
    // A character outside the basic plane -- an emoji, say -- is two code units,
    // and a cut between them leaves the second one on its own, which draws as a
    // replacement glyph. Dropping it costs one character of the oldest text.
    if (isLowSurrogate(live.charCodeAt(start))) {
      start += 1
    }
    liveDropped += start
    live = live.slice(start)
  }
  // Written straight into the existing element rather than by redrawing: a command
  // printing steadily would otherwise rebuild every cell several times a second.
  const element = liveElement(index)
  if (element) {
    element.textContent = live
    // Except while find is showing a match in it: following the newest line
    // would carry the match the reader stepped to out of sight on the next chunk.
    if (!holdsCurrentMatch(element)) {
      scrollToEnd(element)
    }
    // Only this block changed, so only it is searched again.
    scheduleFind(element)
  }
}

/** Whether a code unit is the second half of a character, not one by itself. */
function isLowSurrogate(unit) {
  return unit >= 0xdc00 && unit <= 0xdfff
}

/**
 * The live output element of a cell, if it is on screen.
 *
 * Absent while the first draw after the run started is still in flight, which is
 * harmless: that draw takes `live` as it is by then.
 */
function liveElement(index) {
  if (!Number.isInteger(index)) {
    return null
  }
  return cellsEl.querySelector(`section.cell[data-index="${index}"] pre.live`)
}

/** Redraws from the backend's copy of the document. Reports whether it worked. */
async function refresh() {
  try {
    render(await invoke('document'))
    return true
  } catch (error) {
    setStatus(String(error), 'error')
    return false
  }
}

async function runCell(index) {
  if (busy) {
    return
  }
  setBusy(true)
  try {
    const report = await invoke('run_cell', { index })
    setStatus(
      `Cell ${index + 1} done (${report.status})`,
      report.success ? 'ok' : 'error',
    )
  } catch (error) {
    setStatus(String(error), 'error')
    // A write-back failure means the file changed underneath us, so what is on
    // screen is stale. Pick the file back up rather than leave it stale.
    await reload(true)
  } finally {
    running = null
    live = ''
    liveDropped = 0
    dismissPassword()
    setBusy(false)
    await refresh()
  }
}

async function runAll() {
  if (busy) {
    return
  }
  setBusy(true)
  try {
    const { reports, stopped } = await invoke('run_all')
    const failed = reports.filter((report) => !report.success).length
    setStatus(
      stopped
        ? `Stopped after ${reports.length} cells.`
        : failed === 0
          ? `Ran ${reports.length} cells.`
          : `Ran ${reports.length} cells, ${failed} failed.`,
      failed === 0 ? 'ok' : 'error',
    )
  } catch (error) {
    setStatus(String(error), 'error')
    await reload(true)
  } finally {
    running = null
    live = ''
    liveDropped = 0
    dismissPassword()
    setBusy(false)
    await refresh()
  }
}

async function stop() {
  try {
    // The backend remembers the request for the rest of the operation, so a press
    // that lands between two cells of a batch still stops it.
    const stopped = await invoke('cancel')
    setStatus(stopped ? 'Stopping…' : 'Nothing is running.', 'info')
  } catch (error) {
    setStatus(String(error), 'error')
  }
}

async function reload(quiet) {
  // Re-reading the file can add, remove or reorder cells, and the check is held
  // by index: kept, it would move to whichever cell landed on that index. The TUI
  // drops its per-cell state on reload for the same reason. A write-back
  // (`runandlog://document`) is not this case -- it replaces one result and leaves
  // the cells where they were -- so the check survives that.
  forgetCopied()
  try {
    const doc = await invoke('reload')
    // Again, for a press made while the file was being re-read. Copy stays
    // enabled through a reload, and such a press is numbered after the call
    // above, so only a second one can tell it that the cell it aimed at may not
    // be at that index any more. Nothing can come between this and the draw:
    // both run without yielding.
    forgetCopied()
    render(doc)
    if (!quiet) {
      setStatus('Reloaded.', 'ok')
    }
  } catch (error) {
    setStatus(`Reload failed: ${error}`, 'error')
  }
}

// Find (Cmd+F on macOS, Ctrl+F elsewhere).
//
// The webview has no find of its own -- WKWebView gives an app none unless the
// app builds it -- so the window carries a small one. It searches what is on
// screen: each cell's command and its result, or the live output of the cell
// that is running.
//
// Matches are painted with the CSS Custom Highlight API, which colours ranges of
// text without touching the DOM. Wrapping them in elements would have to be
// undone and redone around every redraw, cut through the spans a command is
// coloured with, and fight `appendLive`, which rewrites the live block on every
// chunk. A webview without the API still finds, counts and scrolls to each
// match; it just cannot colour them.

/** Whether the webview can paint ranges of text (the Custom Highlight API). */
const canHighlight =
  typeof CSS !== 'undefined' &&
  CSS.highlights !== undefined &&
  typeof Highlight === 'function'
/**
 * Most matches kept at once. A one-letter query over a long log can match
 * hundreds of thousands of times, and every match is a Range the webview has to
 * keep up to date and paint. The count says when it stopped.
 */
const FIND_MAX_MATCHES = 5000
/** macOS uses Cmd for the shortcuts, where Ctrl+F is a caret movement. */
const isMac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent)

/**
 * The matches, in document order. Each is `{ cell, part, start, pre, range }`:
 * `cell` is the cell's index, `part` 0 for the command and 1 for the output,
 * and `start` the offset of the match in that block's text (in the live output,
 * in everything the command has printed this run). The first three are
 * what orders matches and what picks the same match out again after a redraw.
 */
let matches = []
/** Index into `matches` of the current match, or -1. */
let currentMatch = -1
/** Whether `matches` stopped at `FIND_MAX_MATCHES`. */
let matchesCapped = false
/** The query compiled, or null when there is nothing to search for. */
let findPattern = null
/**
 * Work waiting for the next frame: `true` to search everything again, or the
 * blocks whose text changed. Batched because live output can arrive many times
 * between two frames, and searching once per frame is all anyone can see.
 */
let findPending = null
let findFrame = null

/** Whether a key press is the platform's Cmd/Ctrl plus `letter`. */
function isShortcut(event, letter) {
  if (event.altKey || typeof event.key !== 'string') {
    return false
  }
  if (event.key.toLowerCase() !== letter) {
    return false
  }
  return isMac
    ? event.metaKey && !event.ctrlKey
    : event.ctrlKey && !event.metaKey
}

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

/**
 * Compiles the query. Matched as plain text and case-insensitively, the way a
 * browser's find does. A regular expression rather than comparing lowercased
 * copies, because lowercasing can change a string's length (`İ` becomes two code
 * units) and the offsets would no longer line up with the text on screen.
 */
function compileQuery(query) {
  return query === '' ? null : new RegExp(escapeRegExp(query), 'giu')
}

/**
 * Where the command's output ends in a written-back result: before the closing
 * fence `render::fenced` puts on its own last line (three or more backticks), or
 * the end of the text when there is none.
 */
function outputEnd(pre) {
  const text = pre.textContent
  const fence = /\n`{3,}$/.exec(text)
  return fence === null ? text.length : fence.index
}

/** Orders two matches (or keys) by where they are in the document. */
function compareKeys(a, b) {
  return a.cell - b.cell || a.part - b.part || a.start - b.start
}

/**
 * The key of the current match, or null. A match in live output also says how
 * far it starts from the end of the output (`fromEnd`); see `restoreCurrent`.
 */
function currentKey() {
  if (currentMatch < 0 || currentMatch >= matches.length) {
    return null
  }
  const { cell, part, start, pre } = matches[currentMatch]
  if (!pre.classList.contains('live')) {
    return { cell, part, start }
  }
  // Trailing newlines are not counted: the written-back result drops them
  // (`render::normalize`).
  const outputLength = pre.textContent.replace(/\n+$/, '').length
  return { cell, part, start, fromEnd: outputLength - (start - liveDropped) }
}

/**
 * Index of the match to make current again after the matches were found anew.
 *
 * Usually the first match at or after the old one's place. The exception is a
 * match in live output whose command has since finished: its block is now the
 * written-back result, which puts a summary line and a fence before the output
 * (or holds only a link, when the output went to a file), so an offset into the
 * live output points at the wrong text in it. The output ends the result -- up to
 * the closing fence -- just as it ended the live block, so the match is found
 * again by its distance from that end (Codex review). Counted in characters
 * rather than in matches: the list of matches can be cut off at
 * `FIND_MAX_MATCHES`, and the result has matches the live block did not.
 */
function restoreCurrent(key) {
  if (key === null) {
    return matches.length > 0 ? 0 : -1
  }
  if (key.fromEnd !== undefined) {
    const stillLive = matches.some(
      (match) => match.cell === key.cell && match.pre.classList.contains('live'),
    )
    if (!stillLive) {
      let target = null
      let firstInResult = -1
      for (let index = 0; index < matches.length; index++) {
        const match = matches[index]
        if (match.cell !== key.cell || match.part !== key.part) {
          continue
        }
        // One result block per cell, so where its output ends is read once.
        target = target ?? outputEnd(match.pre) - key.fromEnd
        if (firstInResult === -1) {
          firstInResult = index
        }
        if (match.start >= target) {
          return index
        }
      }
      if (firstInResult !== -1) {
        return firstInResult
      }
    }
  }
  return matchAtOrAfter(key)
}

/** The blocks of text find looks through, in document order. */
function searchableBlocks() {
  return cellsEl.querySelectorAll('section.cell pre.command, section.cell pre.result')
}

/**
 * Finds the query in one block, appending to `out`. Stops once `out` holds
 * `limit` matches and reports whether it had to.
 *
 * The text of a block is spread over several text nodes -- a command is one span
 * per coloured piece -- so the nodes are joined to search across those seams and
 * each match is mapped back onto the nodes it spans.
 */
function findInBlock(pre, out, limit) {
  const cell = Number(pre.closest('section.cell').dataset.index)
  const part = pre.classList.contains('command') ? 0 : 1
  // Live output is numbered from the start of everything the command printed,
  // not from the start of the tail kept, so that a match keeps its key when the
  // front of the tail is cut away.
  const base = pre.classList.contains('live') ? liveDropped : 0
  const nodes = []
  const starts = []
  let text = ''
  const walker = document.createTreeWalker(pre, NodeFilter.SHOW_TEXT)
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    nodes.push(node)
    starts.push(text.length)
    text += node.data
  }
  findPattern.lastIndex = 0
  let match
  while ((match = findPattern.exec(text)) !== null) {
    if (out.length >= limit) {
      return true
    }
    const start = match.index
    const end = start + match[0].length
    const range = document.createRange()
    const first = nodeAt(starts, start)
    // The node holding the last character, not the one at `end`: a match that
    // ends where a span ends belongs to that span.
    const last = nodeAt(starts, end - 1)
    range.setStart(nodes[first], start - starts[first])
    range.setEnd(nodes[last], end - starts[last])
    out.push({ cell, part, start: base + start, pre, range })
  }
  return false
}

/**
 * Index of the text node holding `offset`: the last one starting at or before
 * it. The last rather than the first, so that an empty node sharing a start with
 * the node after it is skipped.
 */
function nodeAt(starts, offset) {
  let low = 0
  let high = starts.length - 1
  while (low < high) {
    const middle = (low + high + 1) >> 1
    if (starts[middle] <= offset) {
      low = middle
    } else {
      high = middle - 1
    }
  }
  return low
}

/** Searches every block again. */
function findEverywhere() {
  const found = []
  matchesCapped = false
  if (findPattern !== null) {
    for (const pre of searchableBlocks()) {
      if (findInBlock(pre, found, FIND_MAX_MATCHES)) {
        matchesCapped = true
        break
      }
    }
  }
  matches = found
}

/**
 * Searches one block again and puts its matches in place of its old ones. Falls
 * back to searching everything when the block is gone or the cap is in play,
 * where splicing could not say which matches the cap should keep.
 */
function findAgainIn(pre) {
  if (!pre.isConnected || matchesCapped) {
    findEverywhere()
    return
  }
  const fresh = []
  if (findInBlock(pre, fresh, FIND_MAX_MATCHES + 1)) {
    findEverywhere()
    return
  }
  const key = {
    cell: Number(pre.closest('section.cell').dataset.index),
    part: pre.classList.contains('command') ? 0 : 1,
    start: 0,
  }
  let from = 0
  while (from < matches.length && compareKeys(matches[from], key) < 0) {
    from += 1
  }
  let to = from
  while (to < matches.length && matches[to].pre === pre) {
    to += 1
  }
  if (matches.length - (to - from) + fresh.length > FIND_MAX_MATCHES) {
    findEverywhere()
    return
  }
  matches.splice(from, to - from, ...fresh)
}

/** Index of the first match at or after `key`, wrapping round to the first. */
function matchAtOrAfter(key) {
  if (matches.length === 0) {
    return -1
  }
  const index = matches.findIndex((match) => compareKeys(match, key) >= 0)
  return index === -1 ? 0 : index
}

/**
 * Index of the first match not above the top of the list's view, so that a new
 * search starts from what the reader is looking at rather than from the top of
 * the file.
 */
function firstMatchInView() {
  const top = cellsEl.getBoundingClientRect().top
  const index = matches.findIndex(
    (match) => match.range.getBoundingClientRect().bottom >= top,
  )
  return index === -1 ? (matches.length > 0 ? 0 : -1) : index
}

/** Paints the matches and writes the count. */
function paintMatches() {
  if (canHighlight) {
    if (matches.length === 0) {
      CSS.highlights.delete('find-match')
      CSS.highlights.delete('find-current')
    } else {
      CSS.highlights.set(
        'find-match',
        new Highlight(...matches.map((match) => match.range)),
      )
      if (currentMatch >= 0) {
        const current = new Highlight(matches[currentMatch].range)
        // Drawn over the plain match it also is.
        current.priority = 1
        CSS.highlights.set('find-current', current)
      } else {
        CSS.highlights.delete('find-current')
      }
    }
  }
  const more = matchesCapped ? '+' : ''
  findCount.textContent =
    findPattern === null
      ? ''
      : matches.length === 0
        ? 'No matches'
        : `${currentMatch + 1} / ${matches.length}${more}`
  findCount.dataset.empty = String(findPattern !== null && matches.length === 0)
  findPrevButton.disabled = matches.length === 0
  findNextButton.disabled = matches.length === 0
}

/** Whether find's current match is in `pre`. */
function holdsCurrentMatch(pre) {
  return (
    !findBar.hidden &&
    currentMatch >= 0 &&
    currentMatch < matches.length &&
    matches[currentMatch].pre === pre
  )
}

/**
 * Brings the current match into view: first inside its block, if the block
 * scrolls on its own (the live output does), then in the list of cells. Left
 * alone when it is already visible, so stepping through matches on one screen
 * does not keep moving the page.
 */
function revealCurrentMatch(withinBlockOnly = false) {
  if (currentMatch < 0) {
    return
  }
  const { pre, range } = matches[currentMatch]
  for (const box of withinBlockOnly ? [pre] : [pre, cellsEl]) {
    if (box.scrollHeight <= box.clientHeight) {
      continue
    }
    const target = range.getBoundingClientRect()
    const frame = box.getBoundingClientRect()
    if (target.top < frame.top || target.bottom > frame.bottom) {
      box.scrollTop += target.top - frame.top - (box.clientHeight - target.height) / 2
    }
  }
}

/**
 * Searches again after the document changed under an open search: a redraw
 * (`pre` null) or new live output in one block. The current match is kept by its
 * position in the document, and the list does not scroll -- a command printing
 * must not pull the reader around while they read something else.
 */
function scheduleFind(pre) {
  if (findBar.hidden || findPattern === null) {
    return
  }
  if (pre === null || findPending === true) {
    findPending = true
  } else {
    findPending = findPending || new Set()
    findPending.add(pre)
  }
  if (findFrame === null) {
    findFrame = requestAnimationFrame(runPendingFind)
  }
}

function runPendingFind() {
  findFrame = null
  const pending = findPending
  findPending = null
  if (pending === null || findBar.hidden || findPattern === null) {
    return
  }
  const key = currentKey()
  if (pending === true) {
    findEverywhere()
  } else {
    for (const pre of pending) {
      findAgainIn(pre)
    }
  }
  currentMatch = restoreCurrent(key)
  paintMatches()
  // A live block holding the current match no longer follows its end (see
  // `appendLive`), and cutting the front of its tail moves the text up under it.
  // Kept in view inside that block only: the list itself stays where the reader
  // put it.
  if (pending !== true && currentMatch >= 0 && pending.has(matches[currentMatch].pre)) {
    revealCurrentMatch(true)
  }
}

/** Searches for what is in the field now, after it was edited. */
function findQuery() {
  const key = currentKey()
  findPattern = compileQuery(findInput.value)
  findEverywhere()
  // Typing more of a word keeps to the match already shown, as long as it still
  // matches; a fresh search starts from what is in view.
  currentMatch = key === null ? firstMatchInView() : matchAtOrAfter(key)
  paintMatches()
  revealCurrentMatch()
}

/** Moves to the next (`step` 1) or previous (`step` -1) match. */
function stepMatch(step) {
  if (matches.length === 0) {
    return
  }
  currentMatch =
    currentMatch < 0
      ? step > 0
        ? 0
        : matches.length - 1
      : (currentMatch + step + matches.length) % matches.length
  paintMatches()
  revealCurrentMatch()
}

function openFind() {
  const wasHidden = findBar.hidden
  findBar.hidden = false
  findInput.focus()
  findInput.select()
  // Opened again with the last query still in the field: show its matches again,
  // since closing took them away.
  if (wasHidden && findInput.value !== '') {
    currentMatch = -1
    findQuery()
  }
}

function closeFind() {
  findBar.hidden = true
  findPattern = null
  matches = []
  currentMatch = -1
  matchesCapped = false
  findPending = null
  paintMatches()
  findInput.blur()
}

findInput.addEventListener('input', findQuery)
findInput.addEventListener('keydown', (event) => {
  // Enter while an input method is composing (Japanese, say) confirms the
  // composition; it is not a request for the next match.
  if (event.isComposing || event.keyCode === 229) {
    return
  }
  if (event.key === 'Enter') {
    event.preventDefault()
    stepMatch(event.shiftKey ? -1 : 1)
  }
})
findPrevButton.addEventListener('click', () => stepMatch(-1))
findNextButton.addEventListener('click', () => stepMatch(1))
findCloseButton.addEventListener('click', closeFind)

// On the document, so that the shortcut works wherever the focus is. Nothing in
// the app menu holds these keys, so they reach the page.
document.addEventListener('keydown', (event) => {
  // The password prompt is modal; find opens behind it with nowhere to type.
  if (passwordDialog.open || event.isComposing) {
    return
  }
  if (isShortcut(event, 'f') && !event.shiftKey) {
    event.preventDefault()
    openFind()
  } else if (isShortcut(event, 'g') && !findBar.hidden) {
    // Cmd+G / Shift+Cmd+G, the other way every Mac app steps through matches.
    event.preventDefault()
    stepMatch(event.shiftKey ? -1 : 1)
  } else if (event.key === 'Escape' && !findBar.hidden) {
    event.preventDefault()
    closeFind()
  }
})

runAllButton.addEventListener('click', runAll)
stopButton.addEventListener('click', stop)
reloadButton.addEventListener('click', () => reload(false))
passwordForm.addEventListener('submit', (event) => {
  event.preventDefault()
  answerPassword(passwordInput.value)
})
passwordDecline.addEventListener('click', () => answerPassword(null))
// Escape closes a modal dialog on its own; the command still needs its answer.
passwordDialog.addEventListener('cancel', (event) => {
  event.preventDefault()
  answerPassword(null)
})

// Nothing can be started until the events are subscribed. `listen` registers with
// the backend asynchronously, and Stop is only offered once a run has said it
// started -- so a run begun before the subscription exists would miss that event
// and leave Stop unavailable for as long as the command takes.
async function start() {
  setBusy(true)
  setStatus('Starting…', 'info')
  // Settled rather than raced: a rejection from `Promise.all` would leave whatever
  // subscriptions did succeed in place, half-listening. Either all of them are on or
  // none are, so the rest of the window has one state to reason about.
  const attempts = await Promise.allSettled([
    listen('runandlog://document', (event) => {
      // The document arrives once a run has been written back, so the live view of
      // it has served its purpose. Cleared before the draw: left set, the finished
      // result would be hidden behind the output it is made of until the next
      // redraw.
      running = null
      live = ''
      liveDropped = 0
      render(event.payload)
    }),
    listen('runandlog://started', (event) => {
      running = event.payload
      live = ''
      liveDropped = 0
      stopButton.disabled = false
      setStatus(`Running cell ${event.payload + 1}…`, 'info')
      // Redraw so the cell being run shows its spinner label and its live output.
      refresh()
    }),
    listen('runandlog://output', (event) => {
      appendLive(event.payload.index, event.payload.text)
    }),
    listen('runandlog://finished', () => {
      running = null
      // Whatever asked for a password has ended, and the backend has declined for it.
      dismissPassword()
    }),
    listen('runandlog://password', (event) => {
      askPassword(event.payload)
    }),
  ])
  const failure = attempts.find((attempt) => attempt.status === 'rejected')
  subscribed = failure === undefined
  if (subscribed) {
    // Only now can a password request be shown, so only now may the backend send
    // one; until then it declines them for us.
    try {
      await invoke('listen_for_passwords')
    } catch (error) {
      setStatus(String(error), 'error')
    }
  }
  if (!subscribed) {
    // Subscribing can be refused -- by a missing capability, say. The window is
    // then blind to a run's progress, but it can still show the document, run
    // cells and stop them, so it opens in that state rather than saying
    // "Starting…" forever with every button dead.
    // Awaited, and the failures swallowed: until the subscriptions that did take
    // are gone the window is half-listening, which is the state this is here to
    // avoid. One that refuses to come off is not worth stopping the window for --
    // the status already says the events cannot be relied on.
    await Promise.allSettled(
      attempts
        .filter((attempt) => attempt.status === 'fulfilled')
        .map((attempt) => attempt.value()),
    )
  }

  setBusy(false)
  // The status is set after the document is read, and only if reading it worked:
  // "Ready." over the top of a failure would hide the one message explaining an
  // empty window.
  if (await refresh()) {
    setStatus(
      subscribed ? 'Ready.' : `Cannot follow a run: ${failure.reason}`,
      subscribed ? 'info' : 'error',
    )
  }
}

start()
