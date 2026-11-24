#!/usr/bin/env bash
# Compatibility baseline harness.
#
# Usage:
#   ./run.sh record                 Record baseline/ from the reference shells.
#   ./run.sh verify-refs            Re-run references and check they still match baseline/ (no sharky needed).
#   ./run.sh check <candidate>      Run a sharky-style binary and check it against baseline/.
#
# Method (deliberately narrow):
#   * Each fixture runs in a fresh isolated cwd with a cleared environment.
#   * Every run is bounded by a timeout that kills the whole process group
#     (including children spawned by the fixture).
#   * Only stdout, exit status and controlled file side effects are compared.
#   * stderr is captured to /dev/null and NEVER compared between shells.
#   * Portable fixtures use dash; bash-only fixtures use bash.
#
# Explicit configuration (no implicit tool discovery on missing references):
#   DASH_BIN        dash binary for portable cases; required when dash is not in PATH.
#   BASH_BIN        bash binary for bash-only cases (default: bash in PATH).
#   COMPAT_PATH     PATH used inside every isolated run; auto-resolved from the
#                   current tool locations when unset (always includes printf).
#   TIMEOUT_SECS    per-case timeout in seconds (default: 5; SIGKILL 2s later).
#   TIMEOUT_BIN     timeout binary (default: timeout in PATH).
#   CANDIDATE_ARGS  candidate invocation (default: --no-config -c).

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURES="$ROOT/fixtures"
BASELINE="$ROOT/baseline"
MANIFEST="$ROOT/manifest.tsv"

TIMEOUT_SECS="${TIMEOUT_SECS:-5}"
CANDIDATE_ARGS="${CANDIDATE_ARGS:---no-config -c}"

# Resolve the timeout tool: no run is allowed to be unbounded.
TIMEOUT_BIN="${TIMEOUT_BIN:-$(command -v timeout 2>/dev/null || true)}"
[[ -n "$TIMEOUT_BIN" && -x "$TIMEOUT_BIN" ]] || {
  echo "timeout is required; set TIMEOUT_BIN to a GNU/coreutils timeout binary" >&2
  exit 1
}

# Resolve reference shells. Never glob the Nix store: absence needs an explicit override.
if [[ -z "${DASH_BIN:-}" ]] && command -v dash >/dev/null 2>&1; then
  DASH_BIN="$(command -v dash)"
fi
BASH_BIN="${BASH_BIN:-$(command -v bash)}"

# Controlled PATH shared by references and candidate: directories of the tools
# the isolated runs actually need, derived from this environment and overridable.
resolve_tool_path() {
  local tool p dir added=""
  local -a dirs=()
  for tool in printf timeout bash; do
    p="$(type -P "$tool" 2>/dev/null || true)"
    [[ -n "$p" ]] || continue
    dir="$(dirname "$p")"
    [[ "|$added|" == *"|$dir|"* ]] && continue
    dirs+=("$dir"); added="$added|$dir"
  done
  for dir in /usr/bin /bin; do
    [[ -d "$dir" ]] || continue
    [[ "|$added|" == *"|$dir|"* ]] && continue
    dirs+=("$dir"); added="$added|$dir"
  done
  local IFS=:
  printf '%s' "${dirs[*]}"
}
TOOL_PATH="${COMPAT_PATH:-$(resolve_tool_path)}"
[[ -n "$TOOL_PATH" ]] || { echo "empty TOOL_PATH; set COMPAT_PATH" >&2; exit 1; }

# Fixed environment for every invocation. HOME is set per run to the isolated cwd.
ENV_ARGS=(-i "PATH=$TOOL_PATH" LC_ALL=C.UTF-8 DIFF_X='a  b')

declare -a NAMES CLASSES REFS
load_manifest() {
  NAMES=(); CLASSES=(); REFS=()
  local line name class ref
  while IFS=$'\t' read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" || "$line" == \#* ]] && continue
    IFS=$'\t' read -r name class ref <<<"$line"
    NAMES+=("$name"); CLASSES+=("$class"); REFS+=("$ref")
  done < "$MANIFEST"
}

# Execute one case into globals CASE_OUT CASE_STATUS CASE_SNAP.
# runner is "ref" or "candidate"; candidate path in $4.
run_case() {
  local runner="$1" class="$2" src="$3" candidate="${4:-}"
  local cwd out snap
  cwd="$(mktemp -d "${TMPDIR:-/tmp}/sharky-compat.XXXXXX")"
  out="$(mktemp "${TMPDIR:-/tmp}/sharky-compat-out.XXXXXX")"
  snap="$(mktemp -d "${TMPDIR:-/tmp}/sharky-compat-snap.XXXXXX")"

  set +e
  if [[ "$runner" == ref ]]; then
    case "$class" in
      portable)
        if [[ -z "${DASH_BIN:-}" ]]; then
          echo "no dash available; set DASH_BIN explicitly" >&2
          CASE_STATUS=127
        else
          ( cd "$cwd" && "$TIMEOUT_BIN" --kill-after=2s "${TIMEOUT_SECS}s" \
              env "${ENV_ARGS[@]}" HOME="$cwd" "$DASH_BIN" -c "$src" ) >"$out" 2>/dev/null
          CASE_STATUS=$?
        fi
        ;;
      bash)
        ( cd "$cwd" && "$TIMEOUT_BIN" --kill-after=2s "${TIMEOUT_SECS}s" \
            env "${ENV_ARGS[@]}" HOME="$cwd" "$BASH_BIN" --noprofile --norc -c "$src" ) >"$out" 2>/dev/null
        CASE_STATUS=$?
        ;;
      *) CASE_STATUS=2 ;;
    esac
  else
    # shellcheck disable=SC2086
    ( cd "$cwd" && "$TIMEOUT_BIN" --kill-after=2s "${TIMEOUT_SECS}s" \
        env "${ENV_ARGS[@]}" HOME="$cwd" "$candidate" $CANDIDATE_ARGS "$src" ) >"$out" 2>/dev/null
    CASE_STATUS=$?
  fi
  set -e

  # Snapshot controlled side effects: every regular file left in the isolated cwd.
  local f
  while IFS= read -r -d '' f; do
    mkdir -p "$snap/$(dirname "$f")"
    cp -- "$cwd/$f" "$snap/$f"
  done < <(cd "$cwd" && find . -type f -print0 2>/dev/null)

  CASE_OUT="$out"
  CASE_SNAP="$snap"
  CASE_CWD="$cwd"
}

cleanup_case() {
  rm -rf -- "${CASE_CWD:-}" "${CASE_SNAP:-}" "${CASE_OUT:-}"
}

# In check mode the label must identify the candidate, never the reference.
describe_runner() {
  local mode="$1" class="$2" candidate="${3:-}"
  if [[ "$mode" == check ]]; then
    printf '%s %s <source>' "$candidate" "$CANDIDATE_ARGS"
    return
  fi
  case "$class" in
    portable) printf '%s -c <source>' "${DASH_BIN:-dash}" ;;
    bash) printf '%s --noprofile --norc -c <source>' "$BASH_BIN" ;;
    *) printf '%s' "$candidate" ;;
  esac
}

# Preflight: the candidate must actually be able to run printf before results mean anything.
preflight_candidate() {
  local candidate="$1"
  run_case candidate bash "printf 'ok\n'" "$candidate"
  local out
  out="$(cat "$CASE_OUT")"
  if [[ "$CASE_STATUS" -ne 0 || "$out" != "ok" ]]; then
    echo "candidate cannot run printf (status=$CASE_STATUS stdout='$out'); check COMPAT_PATH / candidate invocation" >&2
    cleanup_case
    return 1
  fi
  cleanup_case
}

cmd_record() {
  [[ -n "${DASH_BIN:-}" ]] || { echo "no dash available; set DASH_BIN explicitly" >&2; exit 1; }
  load_manifest
  mkdir -p "$BASELINE"
  echo "dash:  ${DASH_BIN}"
  echo "bash:  ${BASH_BIN}"
  echo "env:   env -i PATH='$TOOL_PATH' LC_ALL=C.UTF-8 DIFF_X='a  b' HOME=<isolated-cwd>"
  echo "timeout: ${TIMEOUT_BIN} --kill-after=2s ${TIMEOUT_SECS}s"
  echo "recording ${#NAMES[@]} cases"
  local i name class src
  for i in "${!NAMES[@]}"; do
    name="${NAMES[$i]}"; class="${CLASSES[$i]}"
    src="$(cat "$FIXTURES/$name.sh")"
    run_case ref "$class" "$src"
    rm -rf -- "$BASELINE/$name.stdout" "$BASELINE/$name.status" "$BASELINE/$name.files"
    mkdir -p "$BASELINE/$name.files"
    cp -- "$CASE_OUT" "$BASELINE/$name.stdout"
    printf '%s\n' "$CASE_STATUS" > "$BASELINE/$name.status"
    if [[ -n "$(ls -A "$CASE_SNAP" 2>/dev/null)" ]]; then
      cp -a -- "$CASE_SNAP/." "$BASELINE/$name.files/"
    fi
    printf '  %-22s %-8s %-4s stdout=%sB files=%s\n' \
      "$name" "$class" "$CASE_STATUS" "$(wc -c < "$BASELINE/$name.stdout")" \
      "$(find "$BASELINE/$name.files" -type f | wc -l)"
    cleanup_case
  done
}

compare_case() {
  local name="$1" fail=0
  local expected_status actual_status expected_files cmp_files
  expected_status="$(cat "$BASELINE/$name.status")"
  actual_status="$CASE_STATUS"
  if ! cmp -s "$CASE_OUT" "$BASELINE/$name.stdout"; then
    echo "  FAIL $name: stdout differs (expected $(cat "$BASELINE/$name.stdout" | od -An -c | tr -d '\n') | got $(od -An -c < "$CASE_OUT" | tr -d '\n'))"
    fail=1
  fi
  if [[ "$expected_status" != "$actual_status" ]]; then
    echo "  FAIL $name: status expected=$expected_status got=$actual_status"
    fail=1
  fi
  # Empty baseline .files/ dirs are not tracked by git, so a missing dir means "no files".
  expected_files="$BASELINE/$name.files"
  cmp_files="$expected_files"
  if [[ ! -d "$expected_files" ]]; then
    cmp_files="$(mktemp -d "${TMPDIR:-/tmp}/sharky-compat-empty.XXXXXX")"
  fi
  if ! diff -r -q "$cmp_files" "$CASE_SNAP" >/dev/null 2>&1; then
    echo "  FAIL $name: side-effect files differ"
    diff -r "$cmp_files" "$CASE_SNAP" 2>&1 | sed 's/^/      /'
    fail=1
  fi
  [[ "$cmp_files" == "$expected_files" ]] || rm -rf -- "$cmp_files"
  return "$fail"
}

cmd_check() {
  local mode="$1" candidate="${2:-}"
  if [[ "$mode" == check && -z "$candidate" ]]; then
    echo "usage: $0 check <candidate-binary>" >&2; exit 2
  fi
  if [[ "$mode" == check ]]; then
    preflight_candidate "$candidate" || exit 2
  fi
  load_manifest
  local i name class src fails=0
  echo "mode: $mode${candidate:+ (candidate: $candidate)}"
  for i in "${!NAMES[@]}"; do
    name="${NAMES[$i]}"; class="${CLASSES[$i]}"
    src="$(cat "$FIXTURES/$name.sh")"
    if [[ "$mode" == verify-refs ]]; then
      run_case ref "$class" "$src"
    else
      run_case candidate "$class" "$src" "$candidate"
    fi
    if compare_case "$name"; then
      printf '  PASS %-22s %s\n' "$name" "$(describe_runner "$mode" "$class" "$candidate")"
    else
      fails=$((fails + 1))
    fi
    cleanup_case
  done
  echo "result: $(( ${#NAMES[@]} - fails ))/${#NAMES[@]} passed"
  [[ "$fails" -eq 0 ]]
}

case "${1:-}" in
  record) cmd_record ;;
  verify-refs) cmd_check verify-refs ;;
  check) cmd_check check "${2:-}" ;;
  *) echo "usage: $0 {record|verify-refs|check <candidate>}" >&2; exit 2 ;;
esac
