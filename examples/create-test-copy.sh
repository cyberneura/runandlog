#!/usr/bin/env bash
# Makes a throwaway copy of exam.md to try runandlog against.
#
# Running a document rewrites it and drops result files next to it, so trying the
# sample straight out of the repository dirties the working tree every time. The
# copy and everything a run writes beside it live in a directory git ignores, and
# each call starts again from a clean copy.
#
#   runandlog "$(examples/create-test-copy.sh)"
#
# The path is the only thing printed on stdout, so it can be substituted straight
# into a command. Everything else goes to stderr.

set -euo pipefail

examples_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
scratch="$examples_dir/scratch"

# A directory of its own per call, rather than one shared name emptied each time:
# emptying it would delete the document a run started earlier is still writing to,
# and anything else left in there. The whole of scratch/ can be deleted whenever.
mkdir -p "$scratch"
run_dir=$(mktemp -d "$scratch/run.XXXXXX")
cp "$examples_dir/exam.md" "$run_dir/exam.md"

echo "made a clean copy in $run_dir" >&2
echo "$run_dir/exam.md"
