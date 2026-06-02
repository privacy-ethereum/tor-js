#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "Usage: $0 <BRANCH>" >&2
  exit 1
fi

git diff "$1"...HEAD --unified=0 --diff-filter=ACMR |
  awk '
    /^diff --git/ { file = substr($3, 3) }
    /^@@/         { match($0, /\+([0-9]+)/, m); line = m[1] }
    /^\+[^+]/     { if (/\<(FIXME|TODO)\>/) printf "%s:%d: %s\n", file, line, substr($0, 2); line++ }
    /^\+$/        { line++ }
  '
