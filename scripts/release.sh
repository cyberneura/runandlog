#!/usr/bin/env bash
# Bumps the version and pushes it. Pushing is what releases.
#
#   scripts/release.sh              # minor bump (0.2.0 -> 0.3.0)
#   scripts/release.sh patch        # 0.2.0 -> 0.2.1
#   scripts/release.sh major        # 0.2.0 -> 1.0.0
#
# The workflow watches Cargo.toml on main and releases any version it has not
# released before, so this script only has to write that version and push it.
# Editing Cargo.toml by hand and pushing does the same thing; this exists to keep
# the three places that carry a version in step.
#
# Nothing needs undoing if the build then fails: the version is not released, so
# fixing the cause and pushing that fix releases it. No version is left stranded
# and none has to be bumped past.

set -euo pipefail

cd "$(dirname "$0")/.."

# One argument at most, and only a known one. `--crates` and `--retry` were both
# real options once; taking a leftover call silently and releasing anyway would be
# the worst of the readings.
if [[ $# -gt 1 ]]; then
    echo "Usage: scripts/release.sh [patch|minor|major]" >&2
    exit 1
fi
BUMP="${1:-minor}"
case "$BUMP" in
    patch | minor | major) ;;
    *)
        echo "Usage: scripts/release.sh [patch|minor|major]" >&2
        exit 1
        ;;
esac

if [[ -n $(git status --porcelain) ]]; then
    echo "Error: the working tree has changes. Commit or stash them first." >&2
    exit 1
fi
branch=$(git rev-parse --abbrev-ref HEAD)
if [[ $branch != "main" ]]; then
    echo "Error: releases are cut from main, not $branch." >&2
    exit 1
fi
git fetch origin main --quiet
if [[ $(git rev-parse HEAD) != $(git rev-parse origin/main) ]]; then
    echo "Error: main and origin/main differ. Push or pull first." >&2
    exit 1
fi

current=$(cargo metadata --format-version 1 --no-deps \
    | jq -r '.packages[] | select(.name == "runandlog") | .version')
IFS=. read -r major minor patch <<< "$current"
case "$BUMP" in
    major) next="$((major + 1)).0.0" ;;
    minor) next="${major}.$((minor + 1)).0" ;;
    patch) next="${major}.${minor}.$((patch + 1))" ;;
esac
echo "$current -> $next"

# The workspace version, which both crates inherit. The line is anchored so that a
# dependency's version is never the one that gets rewritten.
perl -0pi -e "s/^version = \"$current\"\$/version = \"$next\"/m" Cargo.toml
# And the version cli declares for its dependency on core, which has to match or
# `cargo publish` refuses the pair.
perl -0pi -e "s/(runandlog-core = \{ path = \"crates\/runandlog-core\", version = \")$current(\")/\${1}$next\${2}/" Cargo.toml
# The window's manifest carries a version of its own. Nothing reads it while the
# bundler is off, but leaving it behind makes it a lie the moment anyone turns the
# bundler on.
perl -0pi -e "s/(\"version\": \")$current(\")/\${1}$next\${2}/" crates/runandlog-cli/tauri.conf.json
# Bring Cargo.lock along, or the build fails on --locked with the version it still
# remembers.
cargo update --workspace --quiet

# Checked rather than assumed: a reformat or a rename would leave a substitution
# above matching nothing, and a half-bumped tree pushes a commit that releases
# nothing while looking like it should.
metadata=$(cargo metadata --format-version 1 --no-deps)
written=$(jq -r '.packages[] | select(.name == "runandlog") | .version' <<< "$metadata")
if [[ $written != "$next" ]]; then
    echo "Error: Cargo.toml still says $written. Check the substitutions." >&2
    exit 1
fi
# The version cli asks of core, read back through cargo rather than by matching the
# line again: a substitution that missed because the line was reformatted would
# also be missed by a check written the same way. A mismatch here is not caught
# until `cargo publish`, by which point the release is out.
required=$(jq -r '.packages[] | select(.name == "runandlog") | .dependencies[]
    | select(.name == "runandlog-core") | .req' <<< "$metadata")
if [[ $required != "^$next" ]]; then
    echo "Error: runandlog depends on runandlog-core $required, not ^$next." >&2
    exit 1
fi
if ! grep -q "\"version\": \"$next\"" crates/runandlog-cli/tauri.conf.json; then
    echo "Error: tauri.conf.json still does not carry $next." >&2
    exit 1
fi

git add Cargo.toml Cargo.lock crates/runandlog-cli/tauri.conf.json
git commit --quiet -m "Release v$next"
git push --quiet origin main

echo "pushed v$next; the release workflow takes it from here"
echo "  https://github.com/cyberneura/runandlog/actions/workflows/release.yml"
