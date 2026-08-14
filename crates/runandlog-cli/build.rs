//! Build script.
//!
//! Only the GUI needs one: `tauri-build` bakes the app manifest and the assets
//! into the binary. Without the `gui` feature this is a no-op, so a CLI-only
//! build does not need webkit2gtk / gtk3 to be installed.

fn main() {
    #[cfg(feature = "gui")]
    tauri_build::build();
}
