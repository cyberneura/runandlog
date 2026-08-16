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
/** Buttons are disabled while a run is in flight. */
let busy = false
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

/** Draws the document. Called for the first paint and after every write-back. */
function render(doc) {
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

  const command = document.createElement('pre')
  command.className = 'command'
  command.textContent = cell.command

  section.append(head, command)

  if (hasResult) {
    const result = document.createElement('pre')
    result.className = 'result'
    result.textContent = cell.result
    section.append(result)
  }
  return section
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
  try {
    render(await invoke('reload'))
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
  // subscriptions did succeed in place, half-listening. Either all three are on or
  // none are, so the rest of the window has one state to reason about.
  const attempts = await Promise.allSettled([
    listen('runandlog://document', (event) => {
      render(event.payload)
    }),
    listen('runandlog://started', (event) => {
      running = event.payload
      stopButton.disabled = false
      setStatus(`Running cell ${event.payload + 1}…`, 'info')
      // Redraw so the cell being run shows its spinner label.
      refresh()
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
