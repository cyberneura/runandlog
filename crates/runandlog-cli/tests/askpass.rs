//! The askpass helper, end to end: a command runs the helper the environment
//! names, the helper reaches the front end, and the answer comes out on its stdout.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};

use runandlog::askpass::{Askpass, RunInProgress};
use runandlog_core::ExecOptions;

/// Starts a run, as a front end's session does. The token is to be held until the
/// command has ended: the helper refuses prompts from a run that is over.
fn options(askpass: &Askpass) -> (ExecOptions, RunInProgress) {
    let mut options = ExecOptions::new(std::env::temp_dir());
    options.shell = PathBuf::from("/bin/sh");
    let run = askpass.apply(&mut options);
    (options, run)
}

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_runandlog"))
}

#[test]
fn a_command_gets_the_answer_through_the_helper() {
    let (asked_tx, asked_rx) = mpsc::channel();
    let asked_tx = Mutex::new(asked_tx);
    let askpass = Askpass::start_with_helper(
        Box::new(move |prompt, _| {
            asked_tx.lock().unwrap().send(prompt.to_string()).unwrap();
            Some("s3cret".to_string())
        }),
        binary(),
    )
    .unwrap();

    // What sudo -A does: run the helper with the prompt, read its stdout. The
    // helper is only offered where the user has not chosen one, so the check reads
    // whichever variable the environment running the test left to runandlog.
    let (options, _run) = options(&askpass);
    let outcome = runandlog_core::run(
        r#"helper="$RUNANDLOG_ASKPASS_SOCKET"; helper="${helper%/socket}/runandlog-askpass"
answer=$("$helper" '[sudo] password for me: ') && echo "got <$answer>""#,
        &options,
    )
    .unwrap();

    assert_eq!(asked_rx.recv().unwrap(), "[sudo] password for me:");
    assert_eq!(outcome.output, "got <s3cret>\n");
    assert_eq!(outcome.exit_code, Some(0));
}

#[test]
fn a_declined_prompt_fails_the_helper() {
    let askpass = Askpass::start_with_helper(Box::new(|_, _| None), binary()).unwrap();
    let (options, _run) = options(&askpass);
    let outcome = runandlog_core::run(
        r#"helper="${RUNANDLOG_ASKPASS_SOCKET%/socket}/runandlog-askpass"
"$helper" 'Password:' || echo "declined $?""#,
        &options,
    )
    .unwrap();
    assert_eq!(outcome.output, "declined 1\n");
}

#[test]
fn a_helper_of_a_run_that_is_over_is_refused() {
    // What a background process left behind by a finished cell looks like: the
    // command's environment, no run in progress.
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let askpass = Askpass::start_with_helper(
        {
            let asked = asked.clone();
            Box::new(move |_, _| {
                asked.store(true, std::sync::atomic::Ordering::SeqCst);
                Some("s3cret".to_string())
            })
        },
        binary(),
    )
    .unwrap();
    let (options, run) = options(&askpass);
    drop(run);
    let outcome = runandlog_core::run(
        r#"helper="${RUNANDLOG_ASKPASS_SOCKET%/socket}/runandlog-askpass"
"$helper" 'Password:' || echo "declined $?""#,
        &options,
    )
    .unwrap();
    assert_eq!(outcome.output, "declined 1\n");
    assert!(!asked.load(std::sync::atomic::Ordering::SeqCst));
}
