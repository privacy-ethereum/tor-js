#!/bin/bash

set -euo pipefail

cd "$(dirname "$0")"
GIT_ROOT=$(git rev-parse --show-toplevel)

if [ ! -d "$GIT_ROOT/dist" ]; then
    echo "Error: package dist not found at $GIT_ROOT/dist"
    echo "Run: npm run build"
    exit 1
fi

rm -rf dist
cp -a "$GIT_ROOT"/dist dist
echo "*" >dist/.gitignore

python3 -m http.server 8000
