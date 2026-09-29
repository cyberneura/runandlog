//! Core of Run and Log.
//!
//! Extracts shell command cells from Markdown (`parse`), runs them (`exec`), and
//! formats the outcome for writing back into the Markdown (`render`). Commands
//! are coloured for display by `highlight`.
//!
//! File IO is deliberately kept out of this crate. Callers such as the runandlog
//! binary perform it, so that the TUI, the GUI and non-interactive runs can all
//! share the same pure functions.

pub mod exec;
pub mod highlight;
pub mod parse;
pub mod render;

pub use exec::{Canceller, ExecOptions, ExecOutcome, run, run_cancellable, run_streaming};
pub use highlight::{Token, TokenKind, highlight};
pub use parse::{Cell, Document, Edit, splice};
pub use render::{RenderContext, ResultRender, Sidecar, render_result, renumber_result};
