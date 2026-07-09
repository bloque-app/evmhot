#!/usr/bin/env bash
# Shared helpers for evmhot API scripts. Source this file; do not execute directly.

_api_env_loaded() {
  :
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[1]:-${BASH_SOURCE[0]}}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

load_env() {
  if [[ -f "${PROJECT_ROOT}/.envrc" ]]; then
    # shellcheck disable=SC1091
    set -a
    source "${PROJECT_ROOT}/.envrc"
    set +a
  fi
  if [[ -f "${PROJECT_ROOT}/.env" ]]; then
    # shellcheck disable=SC1091
    set -a
    source "${PROJECT_ROOT}/.env"
    set +a
  fi
}

CHAINS_CONFIG="${CHAINS_CONFIG:-${PROJECT_ROOT}/chains.toml}"
PORT="${PORT:-3000}"
API_BASE="${EVMHOT_API_URL:-http://localhost:${PORT}}"

list_chain_names() {
  if [[ ! -f "${CHAINS_CONFIG}" ]]; then
    echo "Error: chains config not found: ${CHAINS_CONFIG}" >&2
    return 1
  fi
  grep '^name = ' "${CHAINS_CONFIG}" | sed 's/name = "\(.*\)"/\1/'
}

# Read a field from the [[chains]] block matching $1 (e.g. rpc_url, block_offset_from_head).
get_chain_field() {
  local chain="$1"
  local field="$2"
  awk -v chain="${chain}" -v field="${field}" '
    /^name = / {
      gsub(/^name = "|"$/, "")
      current = $0
    }
    current == chain && $0 ~ ("^" field " = ") {
      sub(/^[^=]+= /, "")
      gsub(/^"|"$/, "")
      print
      exit
    }
  ' "${CHAINS_CONFIG}"
}

get_chain_block_offset() {
  local chain="$1"
  local offset
  offset="$(get_chain_field "${chain}" "block_offset_from_head")"
  if [[ -z "${offset}" ]]; then
    echo 20
  else
    echo "${offset}"
  fi
}

rpc_block_number() {
  local rpc_url="$1"
  local hex
  hex="$(
    curl -sS -X POST "${rpc_url}" \
      -H "Content-Type: application/json" \
      -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
      | sed -n 's/.*"result"[[:space:]]*:[[:space:]]*"\(0x[^"]*\)".*/\1/p'
  )"
  if [[ -z "${hex}" ]]; then
    echo "Error: failed to fetch block number from ${rpc_url}" >&2
    return 1
  fi
  printf "%d" "${hex}"
}

api_get_block_number() {
  local chain="$1"
  curl -sS "${API_BASE}/block_number?chain=${chain}"
}

api_set_block_number() {
  local chain="$1"
  local block_number="$2"
  curl -sS -X POST "${API_BASE}/block_number" \
    -H "Content-Type: application/json" \
    -d "{\"chain\":\"${chain}\",\"block_number\":${block_number}}"
}

print_usage_header() {
  echo "API: ${API_BASE}"
  echo "Chains config: ${CHAINS_CONFIG}"
  echo
}
