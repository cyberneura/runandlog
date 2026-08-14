//! The Run and Log CLI / TUI itself.
//!
//! The implementation lives in the library so that both the binary and the
//! integration tests can use it.

#[cfg(feature = "gui")]
pub mod gui;
pub mod session;
pub mod tui;
