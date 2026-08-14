//! Run and Log のコア機能。
//!
//! Markdown からシェルコマンドのセルを取り出し (`parse`)、実行し (`exec`)、
//! 実行結果を Markdown へ書き戻す形に整形する (`render`)。
//!
//! ファイルの読み書きは意図的にこのクレートに持たせていない。TUI / GUI / 非対話実行の
//! いずれも同じ純粋関数を使えるようにするため、IO は呼び出し側 (runandlog-cli 等) が行う。

pub mod exec;
pub mod parse;
pub mod render;

pub use exec::{ExecOptions, ExecOutcome, run};
pub use parse::{Cell, Document, Edit, splice};
pub use render::{RenderContext, ResultRender, Sidecar, render_result};
