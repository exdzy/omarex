#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

export PYTHONDONTWRITEBYTECODE=1

usage() {
  cat <<'EOF'
Usage: ./tests/e2e/run_e2e.sh [OPTIONS]

Opaque-box E2E test runner for omarex.

Options:
  --tier <1|2|3|4|all>   Run only the selected tier (default: all)
  --filter <substr>      Run only tests whose id, feature, or name matches <substr>
  --json <path>          Write structured JSON test report to <path>
  --list                 List discovered matching tests without executing
  --keep-tmp             Keep temporary per-run working directory after exit
  -h, --help             Show this help message and exit 0

Binary resolution order:
  1. $OMAREX_BIN (if set and non-empty, must exist and be executable)
  2. /home/mike/Work/omarex/target/release/omarex (if executable)
  3. /usr/bin/omarex (if executable)

Exit codes:
  0  All selected tests passed (or --list succeeded)
  1  One or more selected tests failed or errored
  2  Usage, binary resolution, or harness/discovery error
EOF
}

die_usage() {
  echo "Error: $*" >&2
  exit 2
}

TIER="all"
FILTER=""
JSON_PATH=""
LIST_ONLY=0
KEEP_TMP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --tier)
      [[ $# -ge 2 ]] || die_usage "missing value for --tier"
      TIER="$2"
      shift 2
      ;;
    --tier=*)
      TIER="${1#--tier=}"
      shift
      ;;
    --filter)
      [[ $# -ge 2 ]] || die_usage "missing value for --filter"
      FILTER="$2"
      shift 2
      ;;
    --filter=*)
      FILTER="${1#--filter=}"
      shift
      ;;
    --json)
      [[ $# -ge 2 ]] || die_usage "missing value for --json"
      JSON_PATH="$2"
      shift 2
      ;;
    --json=*)
      JSON_PATH="${1#--json=}"
      shift
      ;;
    --list)
      LIST_ONLY=1
      shift
      ;;
    --keep-tmp)
      KEEP_TMP=1
      shift
      ;;
    --)
      shift
      if [[ $# -gt 0 ]]; then
        die_usage "unexpected positional argument: $1"
      fi
      ;;
    -*)
      die_usage "unknown option: $1"
      ;;
    *)
      die_usage "unexpected positional argument: $1"
      ;;
  esac
done

case "$TIER" in
  1|2|3|4|all) ;;
  *) die_usage "invalid --tier '$TIER' (expected 1, 2, 3, 4, or all)" ;;
esac

if ! command -v python3 >/dev/null 2>&1; then
  die_usage "python3 not found on PATH"
fi

RESOLVED_BIN=""
if [[ -n "${OMAREX_BIN:-}" ]]; then
  if [[ -f "$OMAREX_BIN" && -x "$OMAREX_BIN" ]]; then
    RESOLVED_BIN="$OMAREX_BIN"
  else
    die_usage "OMAREX_BIN='$OMAREX_BIN' does not exist or is not executable"
  fi
elif [[ -f "${REPO_ROOT}/target/release/omarex" && -x "${REPO_ROOT}/target/release/omarex" ]]; then
  RESOLVED_BIN="${REPO_ROOT}/target/release/omarex"
elif [[ -f "/usr/bin/omarex" && -x "/usr/bin/omarex" ]]; then
  RESOLVED_BIN="/usr/bin/omarex"
else
  die_usage "no executable omarex binary found (checked \$OMAREX_BIN, ${REPO_ROOT}/target/release/omarex, /usr/bin/omarex)"
fi

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/omarex-e2e.XXXXXX")"
cleanup() {
  local rc=$?
  if [[ "$KEEP_TMP" -eq 0 && -n "${TMP_ROOT:-}" && -d "$TMP_ROOT" ]]; then
    rm -rf "$TMP_ROOT"
  elif [[ "$KEEP_TMP" -eq 1 && -n "${TMP_ROOT:-}" ]]; then
    echo "Preserved E2E temp root: $TMP_ROOT" >&2
  fi
  return "$rc"
}
trap cleanup EXIT

PY_ARGS=(
  --tier "$TIER"
  --omarex-bin "$RESOLVED_BIN"
  --tmp-root "$TMP_ROOT"
  --cases-dir "${SCRIPT_DIR}/cases"
)

if [[ -n "$FILTER" ]]; then
  PY_ARGS+=(--filter "$FILTER")
fi
if [[ -n "$JSON_PATH" ]]; then
  PY_ARGS+=(--json "$JSON_PATH")
fi
if [[ "$LIST_ONLY" -eq 1 ]]; then
  PY_ARGS+=(--list)
fi
if [[ "$KEEP_TMP" -eq 1 ]]; then
  PY_ARGS+=(--keep-tmp)
fi

PYTHONPATH="$SCRIPT_DIR" exec python3 -B -c 'import sys; from lib.harness import main; raise SystemExit(main(sys.argv[1:]))' "${PY_ARGS[@]}"
