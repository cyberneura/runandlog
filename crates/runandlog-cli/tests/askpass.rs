//! The askpass helper, end to end: a command runs the helper the environment
//! names, the helper reaches the front end, and the answer comes out on its stdout.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};

use runandlog::askpass::Askpass;
use runandlog_core::ExecOptions;

fn options(askpass: &Askpass) -> ExecOptions {
    let mut options = ExecOptions::new(std::env::temp_dir());
    options.shell = PathBuf::from("/bin/sh");
    askpass.apply(&mut options);
    options
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
    let outcome = runandlog_core::run(
        r#"helper="$RUNANDLOG_ASKPASS_SOCKET"; helper="${helper%/socket}/runandlog-askpass"
answer=$("$helper" '[sudo] password for me: ') && echo "got <$answer>""#,
        &options(&askpass),
    )
    .unwrap();

    assert_eq!(asked_rx.recv().unwrap(), "[sudo] password for me:");
    assert_eq!(outcome.output, "got <s3cret>\n");
    assert_eq!(outcome.exit_code, Some(0));
}

#[test]
fn a_declined_prompt_fails_the_helper() {
    let askpass = Askpass::start_with_helper(Box::new(|_, _| None), binary()).unwrap();
    let outcome = runandlog_core::run(
        r#"helper="${RUNANDLOG_ASKPASS_SOCKET%/socket}/runandlog-askpass"
"$helper" 'Password:' || echo "declined $?""#,
        &options(&askpass),
    )
    .unwrap();
    assert_eq!(outcome.output, "declined 1\n");
}
