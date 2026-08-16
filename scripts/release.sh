#!/usr/bin/env bash
# Cuts a release: bumps the version, pushes it, and starts the build.
#
#   scripts/release.sh              # minor bump (0.1.0 -> 0.2.0)
#   scripts/release.sh patch        # 0.1.0 -> 0.1.1
#   scripts/release.sh major        # 0.1.0 -> 1.0.0
#   scripts/release.sh minor --crates   # also publish to crates.io
#   scripts/release.sh --retry      # build the current version again
#
# What happens:
#   1. Refuse to start unless the working tree is clean and main is where the
#      remote has it. A bump commit on top of unrelated work would release it.
#   2. Bump the workspace version, update Cargo.lock, commit and push.
#   3. Start the Release workflow and watch it. It builds macOS (signed and
#      notarized) and Linux, then creates the GitHub Release.
#   4. Print what to do next for Homebrew (scripts/update-cask.sh).
#
# The version is bumped every time on purpose: a workflow started on a version
# that is already released stops at its first step, and nothing after that point
# can produce an artifact anyone can install.
#
# When something before the release fails -- the build, the notarisation -- the
# bump is on main with no release to show for it. `--retry` starts the workflow
# again on that version instead of moving to another one.
#
# When the release is out and only crates.io failed (a missing token, say),
# `--retry` will not take it: the version is published, which is exactly what it
# refuses to redo. Rerun the failed job instead:
#
#   gh run rerun <run-id> --failed

set -euo pipefail

cd "$(dirname "$0")/.."

BUMP="minor"
PUBLISH_CRATES="false"
RETRY="false"
for argument in "$@"; do
    case "$argument" in
        patch | minor | major) BUMP="$argument" ;;
        --crates) PUBLISH_CRATES="true" ;;
        --retry) RETRY="true" ;;
        *)
            echo "Usage: scripts/release.sh [patch|minor|major] [--crates] [--retry]" >&2
            exit 1
            ;;
    esac
done

# Checked before anything is written: a bump pushed to main with no workflow to
# follow it leaves a version that is never released, and the next run would skip
# straight past it.
for tool in gh cargo; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "Error: $tool not found." >&2
        exit 1
    fi
done
if ! gh auth status >/dev/null 2>&1; then
    echo "Error: gh is not authenticated. Run 'gh auth login'." >&2
    exit 1
fi

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

# Three states, and a fourth for "could not tell". A draft is what a run that failed
# partway leaves behind, and reading it as released would turn a retry away; a
# lookup that merely failed is not evidence of absence, and reading it as "nothing
# there" would let an ordinary run bump past a version sitting there unreleased.
#
# `gh release view` is what asks, rather than the REST endpoint for a tag: that one
# answers for published releases only -- a draft has no tag yet -- and a draft is
# exactly the state this has to recognise.
release_state() {
    local errors json
    errors=$(mktemp)
    if json=$(gh release view "v$1" --json isDraft 2>"$errors"); then
        # The field is read as a boolean rather than compared as text: a truncated
        # or empty answer would otherwise come out as "not a draft", which reads as
        # published -- the one state that stops a release from being retried.
        if jq -e '.isDraft == true' <<< "$json" >/dev/null 2>&1; then
            echo "draft"
        elif jq -e '.isDraft == false' <<< "$json" >/dev/null 2>&1; then
            echo "published"
        else
            echo "unknown"
        fi
    elif grep -qi "release not found" "$errors"; then
        echo "none"
    else
        cat "$errors" >&2
        echo "unknown"
    fi
    rm -f "$errors"
}
state=$(release_state "$current")
if [[ $state == "unknown" ]]; then
    echo "Error: could not tell whether v$current is released. Not guessing." >&2
    exit 1
fi

if [[ $RETRY == "true" ]]; then
    if [[ $state == "published" ]]; then
        echo "Error: v$current is already published. Drop --retry to cut a new one." >&2
        exit 1
    fi
    next="$current"
    echo "retrying v$current ($state)"
elif [[ $state != "published" ]] && git log --first-parent --format=%s \
    | grep -xF "Release v$current" >/dev/null; then
    # Asked of the commits this script writes itself, not of the release history.
    # History cannot tell the two apart: with nothing released yet, a first attempt
    # that died before making anything looks the same as a repository that has never
    # released; once there are releases, a stray draft of another version looks like
    # proof that this one came out.
    #
    # The whole first-parent history, not just HEAD and not a fixed number of
    # commits: the usual next move after a failed release is a commit fixing
    # whatever broke, and any cut-off eventually puts the bump out of reach. The
    # subject carries the version, so looking far back cannot match another one.
    # Only the first parent is followed, so a merged branch cannot supply a subject
    # of its own.
    #
    # `grep -q` is avoided deliberately: it stops at the first match, `git log` then
    # dies of SIGPIPE, and under `pipefail` the whole pipeline fails -- so a found
    # match would read as "not found" and the guard would go quiet. Reading to the
    # end and discarding the output costs nothing here.
    echo "Error: v$current was pushed for release but is $state." >&2
    echo "       'scripts/release.sh --retry' builds it again." >&2
    echo "       Bump past it only if you mean to leave it unreleased." >&2
    exit 1
fi

IFS=. read -r major minor patch <<< "$current"
if [[ $RETRY != "true" ]]; then
    case "$BUMP" in
        major) next="$((major + 1)).0.0" ;;
        minor) next="${major}.$((minor + 1)).0" ;;
        patch) next="${major}.${minor}.$((patch + 1))" ;;
    esac
    echo "$current -> $next"
fi

if [[ $RETRY != "true" ]]; then
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
# Bring Cargo.lock along, or the build fails on --locked with the version it
# still remembers.
cargo update --workspace --quiet

git add Cargo.toml Cargo.lock crates/runandlog-cli/tauri.conf.json
git commit --quiet -m "Release v$next"
git push --quiet origin main
echo "pushed the bump"
fi

pushed=$(git rev-parse HEAD)

# Whatever happens from here, the version is on main. Which way it fell decides
# which of the two recoveries applies, and they are opposites -- so every way out
# says so rather than leaving it to be worked out.
report_state() {
    local final status
    final=$(release_state "$next")
    # Whether the run is over decides whether starting another is sensible: a watch
    # that lost its connection says nothing about the build, which may well still be
    # going.
    status="unknown"
    if [[ -n $run_id ]]; then
        status=$(gh run view "$run_id" --json status --jq .status 2>/dev/null || echo "unknown")
    fi
    echo >&2
    [[ -n $run_url ]] && echo "run:    $run_url ($status)" >&2
    echo "v$next: $final" >&2
    if [[ -z $run_id ]]; then
        if [[ $dispatched == "true" ]]; then
            # The dispatch went through and the run could not be found afterwards.
            # A retry here would start a second build of the same version alongside
            # one that is probably already running.
            echo "The workflow was started but its run could not be found. Look for it:" >&2
            echo "  gh run list --workflow release.yml" >&2
            echo "Only retry once you are sure nothing is running." >&2
        else
            # Nothing was ever started, so there is no run to collide with.
            echo "No run was started. The version is pushed; build it with:" >&2
            echo "  scripts/release.sh --retry" >&2
        fi
        return
    fi
    case "$final" in
        published)
            echo "The release is out. If a later step failed (crates.io, say), rerun it:" >&2
            echo "  gh run rerun ${run_id:-<run-id>} --failed" >&2
            ;;
        none | draft)
            if [[ $status != "completed" ]]; then
                echo "The run has not finished ($status). Watch it before starting another:" >&2
                echo "  gh run watch ${run_id:-<run-id>}" >&2
            else
                echo "The release is not out. Build this version again with:" >&2
                echo "  scripts/release.sh --retry" >&2
            fi
            ;;
        *)
            # Not knowing is its own answer. Sending someone to --retry here would
            # have them start a second run over a release that may well exist, or
            # over one that is still going.
            echo "Whether it came out could not be determined. Look at the run and at" >&2
            echo "'gh release view v$next' before starting anything again." >&2
            ;;
    esac
}

# Armed before anything else can fail, and tolerant of a run that is not known yet.
# Every variable the handler reads is set first: under `set -u` an unset one would
# abort the handler itself, taking the recovery instructions with it.
run_id=""
run_url=""
dispatched="false"
trap report_state ERR

# The newest run as it stands, read before dispatching so that ours can be told
# from it. Taken after the dispatch it could already be ours, and the fallback
# below would then rule out the very run it is looking for.
previous=$(gh run list --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId // 0')

# A word of our own, carried through to the run's name. Matching on the commit
# alone is not enough: two dispatches of the same commit are indistinguishable,
# and "the newest run" can be somebody else's, or one queued behind ours.
nonce="$(date +%s)-$$"
# The commit goes with it. `--ref main` resolves when the dispatch lands, so a
# commit that arrives in between would be built and published in place of this one;
# the workflow compares the two and stops rather than release something else.
gh workflow run release.yml --ref main \
    -f publish_crates="$PUBLISH_CRATES" -f dispatch_id="$nonce" -f expected_sha="$pushed"
dispatched="true"

for _ in $(seq 60); do
    sleep 2
    # The commit is part of the match, not just the word: `--ref main` resolves
    # when the dispatch lands, so a main that moved in between would hand this run
    # somebody else's code while still carrying our word.
    run_id=$(gh run list --workflow release.yml --limit 100 \
        --json databaseId,displayTitle,headSha \
        --jq "[.[] | select(.headSha == \"$pushed\" and (.displayTitle | contains(\"$nonce\")))] | .[0].databaseId // empty")
    [[ -n $run_id ]] && break
done

# The word reaches the list through `run-name`, which is one assumption too many to
# rest a release on: if it does not show up there, fall back to the commit and the
# fact that ids only grow. That is less exact -- a second dispatch of the same
# commit would be indistinguishable -- but it beats reporting a healthy run as
# missing.
if [[ -z $run_id ]]; then
    echo "note: no run carried the dispatch word; falling back to matching the commit" >&2
    for _ in $(seq 30); do
        run_id=$(gh run list --workflow release.yml --limit 100 \
            --json databaseId,headSha \
            --jq "[.[] | select(.headSha == \"$pushed\" and .databaseId > $previous)] | min_by(.databaseId) | .databaseId // empty")
        [[ -n $run_id ]] && break
        sleep 2
    done
fi
if [[ -z $run_id ]]; then
    echo "Error: no run appeared carrying $nonce." >&2
    echo "       Check 'gh run list --workflow release.yml'; the version is pushed already," >&2
    echo "       so start it again with 'scripts/release.sh --retry'." >&2
    exit 1
fi
run_url=$(gh run view "$run_id" --json url --jq .url)
echo "watching $run_url"

gh run watch "$run_id" --exit-status
trap - ERR

echo
echo "released v$next: $(gh release view "v$next" --json url --jq .url)"
echo "update Homebrew with: scripts/update-cask.sh"
