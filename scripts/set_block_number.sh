#!/usr/bin/env bash
set -euo pipefail

# Set last-processed block cursor for one chain via POST /block_number.
#
# Usage:
#   ./scripts/set_block_number.sh polygon 89211738
#   ./scripts/set_block_number.sh base 47864100

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/_api_env.sh"

usage() {
  cat <<EOF
Usage: $(basename "$0") CHAIN BLOCK_NUMBER

Set the monitor cursor for CHAIN to BLOCK_NUMBER.

Environment:
  PORT              API port (default: 3000)
  EVMHOT_API_URL    Full API base URL (overrides PORT)
  CHAINS_CONFIG     Path to chains.toml (default: ./chains.toml)

Examples:
  $(basename "$0") polygon 89211738
  $(basename "$0") base 47864100
EOF
}

if [[ $# -ne 2 ]]; then
  usage >&2
  exit 1
fi

chain="$1"
block_number="$2"

if ! [[ "${block_number}" =~ ^[0-9]+$ ]]; then
  echo "Error: BLOCK_NUMBER must be a non-negative integer, got: ${block_number}" >&2
  exit 1
fi

load_env
print_usage_header

before="$(api_get_block_number "${chain}")"
before_block="$(echo "${before}" | sed -n 's/.*"block_number"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')"

response="$(api_set_block_number "${chain}" "${block_number}")"
after_block="$(echo "${response}" | sed -n 's/.*"block_number"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')"

if [[ -z "${after_block}" ]]; then
  echo "Error: failed to set block_number for ${chain}: ${response}" >&2
  exit 1
fi

echo "${chain}: ${before_block:-?} -> ${after_block}"
