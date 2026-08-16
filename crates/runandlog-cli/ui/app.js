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

function setStatus(text, kind) {
  statusEl.textContent = text
  statusEl.dataset.kind = kind || 'info'
}

function setBusy(value) {
  busy = value
  runAllButton.disabled = value
  reloadButton.disabled = value
  // Stop is the one button that only makes sense while a command is running, and
  // it stays off until the backend says one has started: a press that beats the
  // `run_cell` / `run_all` call there finds nothing to stop and is dropped. It is
  // left on between the cells of a batch, where the backend remembers the request.
  if (!value) {
    stopButton.disabled = true
  }
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
  number.textContent = `[${cell.number}]`
  head.append(number)

  const button = document.createElement('button')
  button.className = 'run'
  button.type = 'button'
  // The label is a play triangle plus a word, so the button reads the same way
  // whether or not the glyph renders.
  button.textContent = running === cell.index ? '▶ Running…' : '▶ Run'
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

  if (cell.result !== null && cell.result !== undefined) {
    const result = document.createElement('pre')
    result.className = 'result'
    result.textContent = cell.result
    section.append(result)
  }
  return section
}

async function refresh() {
  try {
    render(await invoke('document'))
  } catch (error) {
    setStatus(String(error), 'error')
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
  await Promise.all([
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
  setBusy(false)
  setStatus('Ready.', 'info')
  await refresh()
}

start()
