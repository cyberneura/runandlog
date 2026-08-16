#!/usr/bin/env bash
# Points the Homebrew cask at the latest release.
#
#   scripts/update-cask.sh           # the version in Cargo.toml
#   scripts/update-cask.sh 0.2.0     # a particular one
#
# The cask lives in another repository (cyberneura/homebrew-tap), so this is done
# from here with your own credentials rather than from CI with a token that can
# write to it.

set -euo pipefail

cd "$(dirname "$0")/.."

TAP="cyberneura/homebrew-tap"
ASSET_HOST="https://github.com/cyberneura/runandlog/releases/download"
# Apple Silicon only, matching what the release workflow builds.
ARCHIVE_TARGET="aarch64-apple-darwin"

if ! command -v gh >/dev/null 2>&1 || ! gh auth status >/dev/null 2>&1; then
    echo "Error: gh is missing or not authenticated." >&2
    exit 1
fi

version="${1:-}"
if [[ -z $version ]]; then
    version=$(cargo metadata --format-version 1 --no-deps \
        | jq -r '.packages[] | select(.name == "runandlog") | .version')
fi
archive="runandlog-v${version}-${ARCHIVE_TARGET}.tar.gz"
echo "cask for v$version ($archive)"

# A draft can be downloaded by whoever owns the repository, and only by them. A
# cask built from one would carry a URL that answers 404 for everybody else, so the
# release has to be out before the tap points at it.
if [[ $(gh release view "v$version" --repo cyberneura/runandlog --json isDraft --jq .isDraft) != "false" ]]; then
    echo "Error: v$version is not published (a draft, or not there at all)." >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The checksum is taken from the asset that is actually published, not from a
# local build: a cask whose sha256 came from a different binary installs nothing.
gh release download "v$version" --repo cyberneura/runandlog \
    --pattern "$archive" --dir "$work"
sha=$(shasum -a 256 "$work/$archive" | cut -d' ' -f1)
echo "sha256 $sha"

gh repo clone "$TAP" "$work/tap" -- --quiet
cask="$work/tap/Casks/runandlog.rb"
cat > "$cask" <<EOF
cask "runandlog" do
  version "$version"
  sha256 "$sha"

  url "$ASSET_HOST/v#{version}/runandlog-v#{version}-$ARCHIVE_TARGET.tar.gz"
  name "Run and Log"
  desc "Runs the shell commands in a Markdown file and writes the results back"
  homepage "https://github.com/cyberneura/runandlog"

  depends_on arch: :arm64

  livecheck do
    url :url
    strategy :github_latest
  end

  # The archive holds a directory, and a cask does not descend into it: the path
  # has to name it. Keep this in step with how the workflow packages the build.
  binary "runandlog-v#{version}-$ARCHIVE_TARGET/runandlog"
end
EOF

git -C "$work/tap" add Casks/runandlog.rb
if git -C "$work/tap" diff --cached --quiet; then
    echo "the cask already points at v$version"
    exit 0
fi
git -C "$work/tap" commit --quiet -m "runandlog $version"
git -C "$work/tap" push --quiet
# Homebrew drops the "homebrew-" prefix when a tap is named on the command line.
echo "pushed the cask; install with: brew install --cask ${TAP/homebrew-/}/runandlog"
