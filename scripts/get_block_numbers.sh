#!/usr/bin/env bash
set -euo pipefail

# Print last-processed block cursor for one or all configured chains.
#
# Usage:
#   ./scripts/get_block_numbers.sh              # all chains in chains.toml
#   ./scripts/get_block_numbers.sh polygon      # single chain

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/_api_env.sh"

load_env
print_usage_header

chains=()
if [[ $# -ge 1 ]]; then
  chains=("$@")
else
  while IFS= read -r chain; do
    chains+=("${chain}")
  done < <(list_chain_names)
fi

printf "%-12s %s\n" "CHAIN" "BLOCK_NUMBER"
printf "%-12s %s\n" "-----" "------------"

for chain in "${chains[@]}"; do
  response="$(api_get_block_number "${chain}")"
  block_number="$(echo "${response}" | sed -n 's/.*"block_number"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')"
  if [[ -z "${block_number}" ]]; then
    echo "Error: failed to read block_number for ${chain}: ${response}" >&2
    exit 1
  fi
  printf "%-12s %s\n" "${chain}" "${block_number}"
done
