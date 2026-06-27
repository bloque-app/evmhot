#!/usr/bin/env bash
set -euo pipefail

# Set block cursors for all chains (or a subset) in one run.
#
# Usage:
#   ./scripts/set_all_block_numbers.sh polygon=89211738 base=47864100
#   ./scripts/set_all_block_numbers.sh --file cursors.txt
#   ./scripts/set_all_block_numbers.sh --from-rpc
#
# cursors.txt format (one per line):
#   polygon 89211738
#   base 47864100
#
# --from-rpc sets each chain to (latest RPC block - block_offset_from_head),
# matching what the monitor uses as its catch-up ceiling.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/_api_env.sh"

usage() {
  cat <<EOF
Usage:
  $(basename "$0") CHAIN=BLOCK [CHAIN=BLOCK ...]
  $(basename "$0") --file PATH
  $(basename "$0") --from-rpc [CHAIN ...]

Examples:
  $(basename "$0") polygon=89211738 base=47864100
  $(basename "$0") --file ./cursors.txt
  $(basename "$0") --from-rpc
  $(basename "$0") --from-rpc polygon
EOF
}

set_chain_block() {
  local chain="$1"
  local block_number="$2"
  local before response after_block

  before="$(api_get_block_number "${chain}")"
  before_block="$(echo "${before}" | sed -n 's/.*"block_number"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')"
  response="$(api_set_block_number "${chain}" "${block_number}")"
  after_block="$(echo "${response}" | sed -n 's/.*"block_number"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')"

  if [[ -z "${after_block}" ]]; then
    echo "Error: failed to set ${chain}: ${response}" >&2
    return 1
  fi
  printf "%-12s %s -> %s\n" "${chain}" "${before_block:-?}" "${after_block}"
}

sync_chain_from_rpc() {
  local chain="$1"
  local rpc_url offset head target

  rpc_url="$(get_chain_field "${chain}" "rpc_url")"
  if [[ -z "${rpc_url}" ]]; then
    echo "Error: no rpc_url for chain ${chain} in ${CHAINS_CONFIG}" >&2
    return 1
  fi

  offset="$(get_chain_block_offset "${chain}")"
  head="$(rpc_block_number "${rpc_url}")"
  target=$((head - offset))

  echo "${chain}: rpc head=${head}, offset=${offset}, setting cursor=${target}"
  set_chain_block "${chain}" "${target}"
}

load_env
print_usage_header

if [[ $# -lt 1 ]]; then
  usage >&2
  exit 1
fi

case "$1" in
  --file)
    if [[ $# -ne 2 ]]; then
      usage >&2
      exit 1
    fi
    file="$2"
    if [[ ! -f "${file}" ]]; then
      echo "Error: file not found: ${file}" >&2
      exit 1
    fi
    while read -r chain block_number _; do
      [[ -z "${chain}" || "${chain}" =~ ^# ]] && continue
      set_chain_block "${chain}" "${block_number}"
    done < "${file}"
    ;;
  --from-rpc)
    shift
    chains=()
    if [[ $# -ge 1 ]]; then
      chains=("$@")
    else
      while IFS= read -r chain; do
        chains+=("${chain}")
      done < <(list_chain_names)
    fi
    for chain in "${chains[@]}"; do
      sync_chain_from_rpc "${chain}"
    done
    ;;
  -h | --help)
    usage
    ;;
  *)
    for pair in "$@"; do
      if [[ "${pair}" != *"="* ]]; then
        echo "Error: expected CHAIN=BLOCK, got: ${pair}" >&2
        exit 1
      fi
      chain="${pair%%=*}"
      block_number="${pair#*=}"
      if ! [[ "${block_number}" =~ ^[0-9]+$ ]]; then
        echo "Error: invalid block number in ${pair}" >&2
        exit 1
      fi
      set_chain_block "${chain}" "${block_number}"
    done
    ;;
esac

echo
echo "Done."
