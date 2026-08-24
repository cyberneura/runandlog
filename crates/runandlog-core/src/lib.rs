//! Core of Run and Log.
//!
//! Extracts shell command cells from Markdown (`parse`), runs them (`exec`), and
//! formats the outcome for writing back into the Markdown (`render`).
//!
//! File IO is deliberately kept out of this crate. Callers such as the runandlog
//! binary perform it, so that the TUI, the GUI and non-interactive runs can all
//! share the same pure functions.

pub mod exec;
pub mod parse;
pub mod render;

pub use exec::{Canceller, ExecOptions, ExecOutcome, run, run_cancellable, run_streaming};
pub use parse::{Cell, Document, Edit, splice};
pub use render::{RenderContext, ResultRender, Sidecar, render_result};
