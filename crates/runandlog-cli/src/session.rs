//! Markdown ファイルと実行状態をまとめて扱う。TUI と非対話実行の共通経路。

use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use runandlog_core::{
    Document, ExecOptions, ExecOutcome, RenderContext, render_result, run, splice,
};

/// 対象の Markdown ファイル 1 つ分の状態。
pub struct Session {
    path: PathBuf,
    doc: Document,
    exec: ExecOptions,
    render: RenderContext,
}

impl Session {
    /// ファイルを読み込む。
    pub fn load(path: &Path, exec: ExecOptions, max_inline_lines: usize) -> io::Result<Session> {
        // シンボリックリンクを解決しておく。解決しないと、書き戻しの rename がリンクを
        // 実ファイルで置き換えてしまう。
        let path = path.canonicalize()?;
        let text = std::fs::read_to_string(&path)?;
        let md_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let md_stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_else(|| "runandlog".to_string());
        Ok(Session {
            doc: Document::parse(&text),
            path,
            exec,
            render: RenderContext {
                md_dir,
                md_stem,
                max_inline_lines,
            },
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn doc(&self) -> &Document {
        &self.doc
    }

    /// セル数。
    pub fn len(&self) -> usize {
        self.doc.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc.cells.is_empty()
    }

    /// セルのコマンド。別スレッドで実行するために取り出す。
    pub fn command_of(&self, index: usize) -> String {
        self.doc.cells[index].command.clone()
    }

    /// 実行時の設定。別スレッドで実行するために取り出す。
    pub fn exec_options(&self) -> ExecOptions {
        self.exec.clone()
    }

    /// セルを実行し、結果を Markdown (と必要なら別ファイル) に書き戻す。
    pub fn run_cell(&mut self, index: usize) -> io::Result<ExecOutcome> {
        let outcome = run(&self.doc.cells[index].command.clone(), &self.exec)?;
        self.apply_outcome(index, &outcome)?;
        Ok(outcome)
    }

    /// 別スレッドで実行した結果を Markdown (と必要なら別ファイル) に書き戻す。
    pub fn apply_outcome(&mut self, index: usize, outcome: &ExecOutcome) -> io::Result<()> {
        self.refresh_before_write()?;
        let cell = self.doc.cells[index].clone();
        let out_file_allowed = match &cell.out_file {
            Some(link) => resolves_inside(&self.render.md_dir, &self.render.md_dir.join(link))?,
            None => true,
        };
        let rendered = render_result(&cell, outcome, &self.render, out_file_allowed);

        if let Some(sidecar) = &rendered.sidecar {
            if let Some(parent) = sidecar.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write_atomically(&sidecar.path, &sidecar.contents)?;
        }
        let updated = splice(
            &self.doc.text,
            vec![self.doc.result_edit(&cell, &rendered.markdown)],
        );
        write_atomically(&self.path, &updated)?;
        self.doc = Document::parse(&updated);
        Ok(())
    }

    /// 書き込みの直前にファイルを読み直す。
    ///
    /// コマンドの実行中に外部エディタで編集されていることがある。実行前の内容で
    /// 上書きするとその編集を黙って捨ててしまうため、読み直したうえで対象のセルが
    /// 変わっていないことを確かめる。
    fn refresh_before_write(&mut self) -> io::Result<()> {
        let current = std::fs::read_to_string(&self.path)?;
        if current == self.doc.text {
            return Ok(());
        }
        let reparsed = Document::parse(&current);
        // セルの並びが 1 つでも変わっていたら、結果を別のセルに書き込みかねないので諦める。
        // 同じ内容のセルが前に挿入された場合、index の一致だけでは取り違えを防げない。
        let commands = |doc: &Document| -> Vec<String> {
            doc.cells.iter().map(|cell| cell.command.clone()).collect()
        };
        if commands(&reparsed) != commands(&self.doc) {
            return Err(io::Error::other(
                "実行中に Markdown が書き換えられたため、結果を書き戻せませんでした。読み直してから再実行してください。",
            ));
        }
        self.doc = reparsed;
        Ok(())
    }

    /// ファイルを読み直す。外部エディタでの編集を反映するため。
    pub fn reload(&mut self) -> io::Result<()> {
        let text = std::fs::read_to_string(&self.path)?;
        self.doc = Document::parse(&text);
        Ok(())
    }
}

/// 一時ファイルへ書いてから rename する。
///
/// 実行中に中断されても、元の Markdown が半端な状態で残らないようにする。
fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "runandlog".to_string());
    // 一時ファイルは必ず新規作成する (create_new)。既存のファイルやシンボリックリンクを
    // 掴んで書き込まないようにするため。名前が衝突した場合は連番をずらして作り直す。
    let mut temp = PathBuf::new();
    let mut file = None;
    for attempt in 0..64 {
        let candidate = path.with_file_name(format!(
            ".{file_name}.runandlog-{}-{attempt}.tmp",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(opened) => {
                temp = candidate;
                file = Some(opened);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let Some(mut file) = file else {
        return Err(io::Error::other("一時ファイルを作成できませんでした"));
    };
    file.write_all(contents.as_bytes())?;
    drop(file);

    // 既存ファイルのパーミッションを引き継ぐ。rename は一時ファイルの属性で置き換えるため。
    if let Ok(metadata) = std::fs::metadata(path) {
        std::fs::set_permissions(&temp, metadata.permissions())?;
    }
    std::fs::rename(&temp, path)
}

/// `target` が (シンボリックリンクを解決したうえで) `base` の配下に収まっているか。
///
/// 途中のディレクトリがリンクだと、`..` を含まないパスでも外へ出られるため、
/// 実際に存在する一番深い祖先まで解決してから比較する。
fn resolves_inside(base: &Path, target: &Path) -> io::Result<bool> {
    let base = base.canonicalize()?;
    let mut existing = target.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|name| name.to_owned()) else {
            return Ok(false);
        };
        let Some(parent) = existing.parent().map(Path::to_path_buf) else {
            return Ok(false);
        };
        rest.push(name);
        existing = parent;
    }
    let mut resolved = existing.canonicalize()?;
    for name in rest.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved.starts_with(&base))
}
