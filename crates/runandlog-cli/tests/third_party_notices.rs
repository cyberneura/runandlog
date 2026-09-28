//! Checks that THIRD-PARTY-NOTICES.txt has kept up with the dependencies.
//!
//! Adding or upgrading a dependency without running
//! `scripts/generate-third-party-notices.sh` is the easiest way for the notices to
//! go stale, and nothing else would notice: the file is only text. These tests
//! compare it with Cargo.lock and fail until it is generated again.
//!
//! The files are read at run time rather than with `include_str!`: Cargo.lock sits
//! outside this crate, and a path that leaves the crate would break
//! `cargo package`.

use std::path::{Path, PathBuf};
use std::process::Command;

use runandlog::notices::THIRD_PARTY_NOTICES;

/// The crates of this workspace. Their licence is LICENSE, not a notice.
const OWN_CRATES: [&str; 2] = ["runandlog", "runandlog-core"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Reads a file with its line endings as LF: a Windows checkout may have made
/// them CRLF.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .replace("\r\n", "\n")
}

/// Names of the dependencies a crate's Cargo.toml ships with: `[dependencies]` and
/// `[target.'cfg(..)'.dependencies]`, one `name = ...` per line. Build and dev
/// dependencies do not end up in the binary.
fn direct_dependencies(manifest: &str) -> Vec<String> {
    let mut section = String::new();
    let mut names = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.to_string();
            continue;
        }
        let shipped = section == "[dependencies]"
            || (section.starts_with("[target.") && section.ends_with(".dependencies]"));
        if !shipped || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            let name = name.trim();
            // `runandlog-core.workspace = true` names the key with a dot.
            let name = name.split('.').next().unwrap_or(name);
            names.push(name.to_string());
        }
    }
    names
}

/// The (name, version) pairs in the "Used by:" blocks.
///
/// An entry is a rule of `=`, `License: ...`, a blank line and "Used by:", and only
/// that sequence opens a block, so a licence text that happens to contain the same
/// words is not counted.
fn crates_in_notices(notices: &str) -> Vec<(String, String)> {
    let separator = "=".repeat(80);
    let mut crates = Vec::new();
    let mut in_block = false;
    let mut after_separator = false;
    let mut after_license = false;
    for line in notices.lines() {
        if in_block {
            let mut words = line.strip_prefix("  ").unwrap_or("").split(' ');
            match (words.next(), words.next()) {
                (Some(name), Some(version)) if !name.is_empty() => {
                    crates.push((name.to_string(), version.to_string()));
                }
                _ => in_block = false,
            }
            continue;
        }
        in_block = after_license && line == "Used by:";
        after_license = (after_separator && line.starts_with("License: "))
            || (after_license && line.is_empty());
        after_separator = line == separator;
    }
    crates
}

/// A `[[package]]` of Cargo.lock: name, version and the lines of its dependencies.
struct Locked {
    name: String,
    version: String,
    dependencies: Vec<String>,
}

fn locked_packages(lock: &str) -> Vec<Locked> {
    lock.split("[[package]]")
        .skip(1)
        .map(|block| {
            let field = |key: &str| {
                block
                    .lines()
                    .find_map(|line| line.strip_prefix(key))
                    .map(|rest| rest.trim().trim_matches('"').to_string())
                    .unwrap_or_default()
            };
            let dependencies = block
                .lines()
                .filter_map(|line| line.strip_prefix(" \""))
                .map(|line| line.trim_end_matches("\",").to_string())
                .collect();
            Locked {
                name: field("name = "),
                version: field("version = "),
                dependencies,
            }
        })
        .collect()
}

/// The version Cargo.lock chose for `dependency` of `owner`. A crate that is in
/// the lock at more than one version is named `name version` in its dependents'
/// lists; otherwise the name alone is there and the one package of that name is it.
fn resolved_version(lock: &[Locked], owner: &str, dependency: &str) -> String {
    let package = lock
        .iter()
        .find(|package| package.name == owner)
        .unwrap_or_else(|| panic!("{owner} is not in Cargo.lock"));
    let entry = package
        .dependencies
        .iter()
        .find(|line| *line == dependency || line.starts_with(&format!("{dependency} ")))
        .unwrap_or_else(|| panic!("{dependency} is not a dependency of {owner} in Cargo.lock"));
    match entry.split(' ').nth(1) {
        Some(version) => version.to_string(),
        None => {
            let mut versions = lock
                .iter()
                .filter(|package| package.name == dependency)
                .map(|package| package.version.clone());
            let version = versions.next().expect("the crate is in Cargo.lock");
            assert!(
                versions.next().is_none(),
                "{dependency} has several versions in Cargo.lock"
            );
            version
        }
    }
}

/// Every direct dependency of both crates is listed, at the version Cargo.lock
/// chose. The name alone would pass with the old version of an upgraded crate
/// still around as somebody's transitive dependency.
#[test]
fn every_direct_dependency_is_listed_at_its_locked_version() {
    // Arrange
    let root = workspace_root();
    let lock = locked_packages(&read(&root.join("Cargo.lock")));
    let listed = crates_in_notices(&THIRD_PARTY_NOTICES.replace("\r\n", "\n"));
    assert!(listed.len() > 100, "parsed from the notices: {listed:?}");
    let owners = [
        ("runandlog", "crates/runandlog-cli/Cargo.toml"),
        ("runandlog-core", "crates/runandlog-core/Cargo.toml"),
    ];

    // Act
    let mut missing = Vec::new();
    for (owner, manifest) in owners {
        let dependencies = direct_dependencies(&read(&root.join(manifest)));
        assert!(
            dependencies.contains(&"libc".to_string()),
            "parsed from {manifest}: {dependencies:?}"
        );
        for name in dependencies {
            if OWN_CRATES.contains(&name.as_str()) {
                continue;
            }
            let entry = (name.clone(), resolved_version(&lock, owner, &name));
            if !listed.contains(&entry) {
                missing.push(entry);
            }
        }
    }

    // Assert
    assert!(
        missing.is_empty(),
        "not in THIRD-PARTY-NOTICES.txt (run scripts/generate-third-party-notices.sh): {missing:?}"
    );
}

/// Nothing is listed that Cargo.lock no longer has: a dependency dropped or
/// upgraded since the notices were generated.
#[test]
fn every_listed_crate_is_in_cargo_lock() {
    // Arrange
    let lock = locked_packages(&read(&workspace_root().join("Cargo.lock")));
    let listed = crates_in_notices(&THIRD_PARTY_NOTICES.replace("\r\n", "\n"));
    assert!(listed.len() > 100, "parsed from the notices: {listed:?}");

    // Act
    let stale: Vec<&(String, String)> = listed
        .iter()
        .filter(|(name, version)| {
            !lock
                .iter()
                .any(|package| &package.name == name && &package.version == version)
        })
        .collect();

    // Assert
    assert!(
        stale.is_empty(),
        "not in Cargo.lock (run scripts/generate-third-party-notices.sh): {stale:?}"
    );
}

/// Run and Log's own crates are not third parties.
#[test]
fn our_own_crates_are_not_listed() {
    // Act
    let listed = crates_in_notices(&THIRD_PARTY_NOTICES.replace("\r\n", "\n"));

    // Assert
    let own: Vec<&(String, String)> = listed
        .iter()
        .filter(|(name, _)| OWN_CRATES.contains(&name.as_str()))
        .collect();
    assert!(own.is_empty(), "{own:?}");
}

/// The copies the crate embeds are the files at the top of the repository.
#[test]
fn the_embedded_texts_are_the_repository_files() {
    // Arrange
    let root = workspace_root();

    // Assert
    assert_eq!(
        runandlog::notices::LICENSE.replace("\r\n", "\n"),
        read(&root.join("LICENSE"))
    );
    assert_eq!(
        THIRD_PARTY_NOTICES.replace("\r\n", "\n"),
        read(&root.join("THIRD-PARTY-NOTICES.txt"))
    );
}

/// `--license` needs no Markdown file, prints our licence and the notices, and
/// exits 0.
#[test]
fn license_flag_prints_the_licences_and_exits_zero() {
    // Act
    let output = Command::new(env!("CARGO_BIN_EXE_runandlog"))
        .arg("--license")
        .output()
        .expect("runandlog starts");

    // Assert
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("the text is UTF-8");
    assert!(stdout.contains("MIT License"));
    assert!(stdout.contains("Copyright (c) 2026 Cyberneura"));
    assert!(stdout.contains("THIRD-PARTY NOTICES"));
    assert!(stdout.contains("\nUsed by:\n"));
}

/// `--license` is a request on its own: it does not quietly drop a file or a run.
#[test]
fn license_flag_refuses_other_arguments() {
    // Act
    let output = Command::new(env!("CARGO_BIN_EXE_runandlog"))
        .args(["--license", "notes.md"])
        .output()
        .expect("runandlog starts");

    // Assert
    assert!(!output.status.success(), "{output:?}");
}
