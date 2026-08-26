#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
web_dir="$(cd "$script_dir/../web" && pwd)"

npm --prefix "$web_dir" ci
npm --prefix "$web_dir" run build
