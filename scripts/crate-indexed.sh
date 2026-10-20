#!/usr/bin/env bash
# Prints whether a crate version is in the crates.io sparse index: yes, no, or
# unknown:<reason>. Used by .github/workflows/release.yml.
#
#   scripts/crate-indexed.sh piigrep 0.1.0
#
# The sparse index, not the web API: it is what cargo resolves against, and the
# two do not update together. Only 200 and 404 are answers; a rate limit, an
# outage or a broken connection is not evidence of absence, and reading it as
# "no" would publish a version a second time.
set -euo pipefail

name=$1
version=$2

# The path nests by name length, and the short cases are not the shape of the
# common one. Getting it wrong yields a 404, which reads as "not published".
case ${#name} in
  1) path="1/$name" ;;
  2) path="2/$name" ;;
  3) path="3/${name:0:1}/$name" ;;
  *) path="${name:0:2}/${name:2:2}/$name" ;;
esac

body=$(mktemp)
trap 'rm -f "$body"' EXIT
# Bounded: a stalled transfer would otherwise hold the plan job, and every job
# behind it, until its timeout. Timing out reads as unknown, like any failure.
status=$(curl -s --connect-timeout 10 --max-time 30 -o "$body" -w '%{http_code}' \
  "https://index.crates.io/$path" || echo 000)
case "$status" in
  404) echo "no" ;;
  200)
    # A 200 whose body will not parse is a broken answer, not a "no". The index
    # is one JSON object per line.
    if ! jq -e . < "$body" >/dev/null 2>&1; then
      echo "unknown:unparsable"
    elif jq -e --arg v "$version" 'select(.vers == $v)' < "$body" >/dev/null; then
      echo "yes"
    else
      echo "no"
    fi
    ;;
  *) echo "unknown:$status" ;;
esac
