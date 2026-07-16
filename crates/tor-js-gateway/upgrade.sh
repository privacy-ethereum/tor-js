#!/usr/bin/env bash
set -euo pipefail

cargo install --path .
tor-js-gateway uninstall
tor-js-gateway install
