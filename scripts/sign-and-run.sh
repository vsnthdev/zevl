#!/usr/bin/env bash
# cargo runner: signs the freshly built binary before executing it.

set -euo pipefail

binary="${1:?usage: sign-and-run.sh <binary> [args...]}"
shift

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$script_dir/sign.sh" "$binary"

exec "$binary" "$@"
