//! Holds a Markdown file together with its run state. Shared by the TUI and
//! non-interactive runs.

use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use runandlog_core::{
    Canceller, Document, ExecOptions, ExecOutcome, RenderContext, Sidecar, render_result,
    run_cancellable, splice,
};

/// State for a single Markdown file.
pub struct Session {
    path: PathBuf,
    doc: Document,
    exec: ExecOptions,
    render: RenderContext,
}

impl Session {
    /// Loads the file.
    pub fn load(path: &Path, exec: ExecOptions, max_inline_lines: usize) -> io::Result<Session> {
        // Resolve symlinks up front. Without this, the rename used to write back
        // would replace the link with a regular file.
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

    /// Number of cells.
    pub fn len(&self) -> usize {
        self.doc.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc.cells.is_empty()
    }

    /// The command of a cell, taken out so it can be run on another thread.
    pub fn command_of(&self, index: usize) -> String {
        self.doc.cells[index].command.clone()
    }

    /// The execution settings, taken out so a run can happen on another thread.
    pub fn exec_options(&self) -> ExecOptions {
        self.exec.clone()
    }

    /// Runs a cell and writes the result back to the Markdown (and to a separate
    /// file when needed).
    ///
    /// Nothing but its timeout can stop the command. Use [`Session::run_cell_cancellable`]
    /// where the user needs a way out.
    pub fn run_cell(&mut self, index: usize) -> io::Result<ExecOutcome> {
        self.run_cell_cancellable(index, &Canceller::new())
    }

    /// Runs a cell, watching `canceller` for a request to stop.
    ///
    /// A cancelled run still has the output it managed to produce written back.
    pub fn run_cell_cancellable(
        &mut self,
        index: usize,
        canceller: &Canceller,
    ) -> io::Result<ExecOutcome> {
        let outcome = run_cancellable(
            &self.doc.cells[index].command.clone(),
            &self.exec,
            canceller,
        )?;
        self.apply_outcome(index, &outcome)?;
        Ok(outcome)
    }

    /// Writes back a result produced on another thread, to the Markdown (and to a
    /// separate file when needed).
    pub fn apply_outcome(&mut self, index: usize, outcome: &ExecOutcome) -> io::Result<()> {
        self.refresh_before_write()?;
        let cell = self.doc.cells[index].clone();
        let out_file_allowed = match &cell.out_file {
            Some(link) => {
                let target = self.render.md_dir.join(link);
                // A destination that is already a directory (`out=logs`, `out=.`)
                // cannot hold the output: the rename in write_atomically would fail
                // and the result would be lost. Treat it like any other rejected
                // designation so the output still lands inline.
                !target.is_dir() && is_safe_out_file(&self.render.md_dir, &self.path, &target)?
            }
            None => true,
        };
        let mut rendered = render_result(&cell, outcome, &self.render, out_file_allowed);

        // The command has already run by this point, so a sidecar that cannot be
        // written must never cost us its output. Rather than enumerating the ways a
        // path can be unusable -- the name taken by a directory, an ancestor that is
        // a regular file, permissions -- just try it and fall back to inline on any
        // failure. This also covers the auto-numbered destination, which is
        // generated rather than designated and so never passed the check above.
        let sidecar_failed = match rendered.sidecar.as_ref() {
            Some(sidecar) => write_sidecar(sidecar).is_err(),
            None => false,
        };
        if sidecar_failed {
            let inline = RenderContext {
                max_inline_lines: usize::MAX,
                ..self.render.clone()
            };
            rendered = render_result(&cell, outcome, &inline, false);
            // Falling back cannot itself need a sidecar, but if it somehow did, the
            // output would be dropped silently. Assert the invariant instead.
            debug_assert!(rendered.sidecar.is_none());
        }

        let updated = splice(
            &self.doc.text,
            vec![self.doc.result_edit(&cell, &rendered.markdown)],
        );
        write_atomically(&self.path, &updated)?;
        self.doc = Document::parse(&updated);
        Ok(())
    }

    /// Re-reads the file just before writing.
    ///
    /// An external editor may have modified it while the command was running.
    /// Overwriting with the pre-run content would silently discard those edits, so
    /// the file is re-read and the cells are checked to be unchanged.
    fn refresh_before_write(&mut self) -> io::Result<()> {
        let current = std::fs::read_to_string(&self.path)?;
        if current == self.doc.text {
            return Ok(());
        }
        let reparsed = Document::parse(&current);
        // Give up if the sequence of cells changed at all: the result could land on
        // the wrong cell. Matching on index alone cannot catch an identical cell
        // being inserted ahead of this one.
        let commands = |doc: &Document| -> Vec<String> {
            doc.cells.iter().map(|cell| cell.command.clone()).collect()
        };
        if commands(&reparsed) != commands(&self.doc) {
            return Err(io::Error::other(
                "the Markdown changed while the command was running, so the result could not be written back; reload and run again",
            ));
        }
        self.doc = reparsed;
        Ok(())
    }

    /// Re-reads the file, picking up edits made in an external editor.
    pub fn reload(&mut self) -> io::Result<()> {
        let text = std::fs::read_to_string(&self.path)?;
        self.doc = Document::parse(&text);
        Ok(())
    }
}

/// Writes a sidecar file, creating the directories leading to it.
fn write_sidecar(sidecar: &Sidecar) -> io::Result<()> {
    if let Some(parent) = sidecar.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomically(&sidecar.path, &sidecar.contents)
}

/// Writes to a temporary file and renames it into place.
///
/// This keeps the original Markdown from being left half-written if the process
/// is interrupted.
fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "runandlog".to_string());
    // The temporary file is always newly created (create_new) so that writing can
    // never land on an existing file or symlink. On a name collision, the counter
    // is bumped and creation is retried.
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
    let Some(file) = file else {
        return Err(io::Error::other("could not create a temporary file"));
    };

    // Once the temporary file exists, every path out of here has to remove it.
    // Leaking it is not merely untidy: the callers now fall back rather than
    // failing, so a repeatedly failing write would leave a temporary behind each
    // time until all 64 candidate names are taken -- at which point writing a
    // sidecar stops working even after whatever caused the failure is cleared.
    let result = finish_atomic_write(file, &temp, path, contents);
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn finish_atomic_write(
    mut file: std::fs::File,
    temp: &Path,
    path: &Path,
    contents: &str,
) -> io::Result<()> {
    file.write_all(contents.as_bytes())?;
    drop(file);

    // Carry over the permissions of the existing file, since the rename replaces
    // them with those of the temporary file.
    if let Ok(metadata) = std::fs::metadata(path) {
        std::fs::set_permissions(temp, metadata.permissions())?;
    }
    std::fs::rename(temp, path)
}

/// Resolves `target` as far as the filesystem allows.
///
/// The path need not exist yet, so the deepest ancestor that does exist is
/// canonicalized and the remaining components are appended to it. Resolving
/// matters because a symlinked directory along the way lets even a path without
/// `..` land somewhere else entirely.
fn resolve_as_far_as_possible(target: &Path) -> io::Result<Option<PathBuf>> {
    let mut existing = target.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|name| name.to_owned()) else {
            return Ok(None);
        };
        let Some(parent) = existing.parent().map(Path::to_path_buf) else {
            return Ok(None);
        };
        rest.push(name);
        existing = parent;
    }
    let mut resolved = existing.canonicalize()?;
    for name in rest.iter().rev() {
        resolved.push(name);
    }
    Ok(Some(resolved))
}

/// Whether writing the output to `target` is safe.
///
/// Two things disqualify a destination:
///
/// - it resolves outside `base`, the directory holding the Markdown file
/// - it resolves to `markdown` itself. `apply_outcome` writes the sidecar first
///   and the document second, so this would replace the document with the command
///   output and then overwrite it again -- losing the sidecar in the good case,
///   and leaving the document *as raw command output* if anything fails in
///   between.
fn is_safe_out_file(base: &Path, markdown: &Path, target: &Path) -> io::Result<bool> {
    let base = base.canonicalize()?;
    let Some(resolved) = resolve_as_far_as_possible(target)? else {
        return Ok(false);
    };
    Ok(resolved.starts_with(&base) && resolved != markdown)
}
