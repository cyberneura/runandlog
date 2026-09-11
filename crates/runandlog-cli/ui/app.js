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
}

function renderCell(cell) {
  const section = document.createElement('section')
  section.className = 'cell'
  section.dataset.index = String(cell.index)
  if (running === cell.index) {
    section.classList.add('running')
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
  const hasResult = cell.result !== null && cell.result !== undefined
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
  command.textContent = cell.command

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
    setStatus(`Could not copy: ${error}`, 'error')
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
    live = live.slice(start)
  }
  // Written straight into the existing element rather than by redrawing: a command
  // printing steadily would otherwise rebuild every cell several times a second.
  const element = liveElement(index)
  if (element) {
    element.textContent = live
    scrollToEnd(element)
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

runAllButton.addEventListener('click', runAll)
stopButton.addEventListener('click', stop)
reloadButton.addEventListener('click', () => reload(false))

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
      render(event.payload)
    }),
    listen('runandlog://started', (event) => {
      running = event.payload
      live = ''
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
    }),
  ])
  const failure = attempts.find((attempt) => attempt.status === 'rejected')
  subscribed = failure === undefined
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
