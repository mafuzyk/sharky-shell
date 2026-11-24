#!/usr/bin/env bash
# agent-session-demo.sh
#
# Dogfood measurement: one persistent `sharky agent --stdio --no-config`
# session vs N independent `bash -c` invocations doing the same recon batch.
#
# This is a MANUAL demo, not part of `cargo test`: it measures and validates a
# live session but the test suite neither runs nor depends on it.
#
# No dependencies beyond bash, python3 (JSON parsing/validation only) and core
# utilities. Run with:  bash scripts/agent-session-demo.sh
# Override paths with REPO / SHARKY_BIN / BASH_BIN / PYTHON environment vars.
#
# The script never edits the repository: every command sent to the agent is
# read-only (variable assignment, cd, pwd, git status/diff, find, echo).

set -u

# Resolve both defaults relative to this script, so the demo runs from any cwd.
# Existence/executability is checked below; no fallback to a built binary.
SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO=${REPO:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
SHARKY_BIN=${SHARKY_BIN:-$REPO/rust/target/debug/sharky}
BASH_BIN=${BASH_BIN:-/run/current-system/sw/bin/bash}
PYTHON=${PYTHON:-python3}
READ_TIMEOUT=${READ_TIMEOUT:-5}

if [[ ! -d "$REPO" || ! -x "$SHARKY_BIN" || ! -x "$BASH_BIN" ]]; then
    printf 'FATAL: REPO=%s SHARKY_BIN=%s BASH_BIN=%s\n' "$REPO" "$SHARKY_BIN" "$BASH_BIN" >&2
    exit 2
fi
cd "$REPO" || exit 2

TMP=$(mktemp -d "${TMPDIR:-/tmp}/agent-demo.XXXXXX") || exit 2
AGENT_PID=
cleanup() {
    [[ -n $AGENT_PID ]] && kill -0 "$AGENT_PID" 2>/dev/null && kill "$AGENT_PID" 2>/dev/null
    exec 3>&- 2>/dev/null || :
    exec 4<&- 2>/dev/null || :
    rm -rf "$TMP"
}
trap cleanup EXIT

# Microsecond clock without spawning a subprocess. EPOCHREALTIME uses a comma
# as decimal separator under some locales and bash arithmetic reads ',' as the
# comma operator, so strip every non-digit before doing arithmetic.
now_us() { local e=$EPOCHREALTIME; printf '%s' "${e//[^0-9]/}"; }
ms_between() { printf '%s' "$(( ($2 - $1) / 1000 ))"; }

# The recon batch, in order. Single quotes keep $... and ~... literal so the
# shell running THIS script does not expand them before they reach the agent.
CMDS=(
    'x=1'
    'cd rust'
    'pwd'
    'git -C .. status --short | head -5'
    'git -C .. diff --stat 03be2a4..HEAD | tail -2'
    "find src -name '*.rs' | head -5"
    'false | true; echo "ps=${PIPESTATUS[@]}"'
    'echo $((0x10 + (1<<3)))'
    'echo ~root'
)
N=${#CMDS[@]}

json_escape() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    printf '%s' "$s"
}

printf '=== Sharky agent session vs %d fresh bash -c calls ===\n' "$N"
printf 'repo:   %s\n' "$REPO"
printf 'sharky: %s\n' "$SHARKY_BIN"
printf 'bash:   %s\n' "$BASH_BIN"
printf '\n'

# ---------------------------------------------------------------- session ---
mkfifo "$TMP/in" "$TMP/out"
"$SHARKY_BIN" agent --stdio --no-config <"$TMP/in" >"$TMP/out" 2>"$TMP/stderr" &
AGENT_PID=$!
exec 3>"$TMP/in"     # write requests to the agent's stdin
exec 4<"$TMP/out"    # read responses from the agent's stdout

: >"$TMP/responses.jsonl"
: >"$TMP/roundtrip.tsv"

SESSION_START=$(now_us)
if IFS= read -r -t "$READ_TIMEOUT" ready <&4 && [[ -n $ready ]]; then
    printf '%s\n' "$ready" >>"$TMP/responses.jsonl"
else
    printf 'FATAL: no ready line within %ss (stderr follows)\n' "$READ_TIMEOUT" >&2
    cat "$TMP/stderr" >&2
    exit 2
fi

for ((i = 1; i <= N; i++)); do
    id="r$i"
    esc=$(json_escape "${CMDS[$((i-1))]}")
    req=$(printf '{"schema_version":1,"id":"%s","op":"exec","command":"%s"}' "$id" "$esc")
    t0=$(now_us)
    printf '%s\n' "$req" >&3
    if IFS= read -r -t "$READ_TIMEOUT" resp <&4 && [[ -n $resp ]]; then
        t1=$(now_us)
        printf '%s\n' "$resp" >>"$TMP/responses.jsonl"
        printf '%s\t%s\n' "$id" "$(ms_between "$t0" "$t1")" >>"$TMP/roundtrip.tsv"
    else
        printf 'FATAL: no response for %s within %ss (stderr follows)\n' "$id" "$READ_TIMEOUT" >&2
        cat "$TMP/stderr" >&2
        exit 2
    fi
done

# Close the session; confirm it ends cleanly.
printf '%s\n' '{"schema_version":1,"id":"close1","op":"close"}' >&3
if IFS= read -r -t "$READ_TIMEOUT" close_resp <&4 && [[ -n $close_resp ]]; then
    printf '%s\n' "$close_resp" >>"$TMP/responses.jsonl"
else
    printf 'WARN: no close acknowledgement within %ss\n' "$READ_TIMEOUT" >&2
fi
SESSION_END=$(now_us)
exec 3>&-
exec 4<&-
wait "$AGENT_PID"
AGENT_EXIT=$?
AGENT_PID=

SESSION_MS=$(ms_between "$SESSION_START" "$SESSION_END")
SESSION_BYTES=$(wc -c <"$TMP/responses.jsonl")
SESSION_BYTES=${SESSION_BYTES//[[:space:]]/}

printf -- '-- session (1 process, %d protocol requests incl. close) --\n' "$((N+1))"
printf 'wall_ms:          %s\n' "$SESSION_MS"
printf 'json_bytes:       %s\n' "$SESSION_BYTES"
printf 'agent_exit:       %s\n' "$AGENT_EXIT"
printf '\n'

# ------------------------------------------------------- bash -c baseline ---
# Each command runs in its OWN fresh bash process from the same starting cwd.
# State (x=1, cd rust) cannot carry over: that difference is the point.
: >"$TMP/bash_output.bin"
: >"$TMP/bash_times.tsv"
BASH_START=$(now_us)
for ((i = 1; i <= N; i++)); do
    b0=$(now_us)
    out=$(cd "$REPO" && "$BASH_BIN" -c "${CMDS[$((i-1))]}" 2>/dev/null)
    b1=$(now_us)
    printf '%s\n' "$out" >>"$TMP/bash_output.bin"
    printf '%s\t%s\n' "r$i" "$(ms_between "$b0" "$b1")" >>"$TMP/bash_times.tsv"
done
BASH_END=$(now_us)
BASH_MS_9=$(ms_between "$BASH_START" "$BASH_END")
BASH_BYTES_9=$(wc -c <"$TMP/bash_output.bin")
BASH_BYTES_9=${BASH_BYTES_9//[[:space:]]/}

# The brief also asks for a "10 calls" figure. Run one extra independent call
# (a trailing pwd) and report both totals so the reader can pick a fair basis.
b0=$(now_us)
out=$(cd "$REPO" && "$BASH_BIN" -c 'pwd' 2>/dev/null)
b1=$(now_us)
printf '%s\n' "$out" >>"$TMP/bash_output.bin"
BASH_MS_10=$(( BASH_MS_9 + $(ms_between "$b0" "$b1") ))
BASH_BYTES_10=$(wc -c <"$TMP/bash_output.bin")
BASH_BYTES_10=${BASH_BYTES_10//[[:space:]]/}
printf '%s\t%s\n' 'r10-extra' "$(ms_between "$b0" "$b1")" >>"$TMP/bash_times.tsv"

printf -- '-- bash -c baseline (one fresh process per call) --\n'
printf 'wall_ms_9:        %s   (%d processes)\n' "$BASH_MS_9" "$N"
printf 'wall_ms_10:       %s   (%d processes, extra pwd)\n' "$BASH_MS_10" "$((N+1))"
printf 'raw_bytes_9:      %s\n' "$BASH_BYTES_9"
printf 'raw_bytes_10:     %s\n' "$BASH_BYTES_10"
printf '\n'

# --------------------------------------------------- validation + table ----
"$PYTHON" - "$TMP/responses.jsonl" "$TMP/roundtrip.tsv" "$TMP/bash_times.tsv" \
    "$REPO" "$SESSION_MS" "$BASH_MS_9" "$SESSION_BYTES" "$BASH_BYTES_9" <<'PY'
import json, sys, os

resp_path, rt_path, bt_path, repo, session_ms, bash_ms, sbytes, bbytes = sys.argv[1:9]

def load_lines(path):
    with open(path, encoding="utf-8") as fh:
        return [ln.rstrip("\n") for ln in fh if ln.strip()]

def load_tsv(path):
    out = {}
    with open(path, encoding="utf-8") as fh:
        for ln in fh:
            parts = ln.rstrip("\n").split("\t")
            if len(parts) == 2:
                out[parts[0]] = int(parts[1])
    return out

raw = load_lines(resp_path)
rtt = load_tsv(rt_path)
bt = load_tsv(bt_path)

checks = []
def check(name, ok, detail=""):
    checks.append((name, bool(ok), detail))

parsed = []
parse_ok = True
for idx, line in enumerate(raw):
    try:
        parsed.append(json.loads(line))
    except Exception as exc:                       # noqa: BLE001 - report it
        parse_ok = False
        check(f"line {idx} parses as JSON", False, str(exc))
        parsed.append(None)
check("all response lines parse as JSON", parse_ok, f"{len(raw)} lines")

ready = parsed[0] if parsed else None
check("first response is ready", isinstance(ready, dict) and ready.get("type") == "ready")
check("ready.schema_version == 1", isinstance(ready, dict) and ready.get("schema_version") == 1)
if isinstance(ready, dict):
    ops = (ready.get("protocol") or {}).get("ops", [])
    check("ready lists exec and close ops", "exec" in ops and "close" in ops, str(ops))

results = [p for p in parsed[1:] if isinstance(p, dict) and p.get("type") == "result"]
close = parsed[-1] if parsed else None
check("last response closes the session",
      isinstance(close, dict) and
      (close.get("type") == "closed" or close.get("outcome") == "closed"),
      str(close) if isinstance(close, dict) else "missing")

expected = [
    ("x=1", None),
    ("cd rust", None),
    ("pwd", repo + "/rust\n"),
    ("git -C .. status --short | head -5", None),
    ("git -C .. diff --stat 03be2a4..HEAD | tail -2", None),
    ("find src -name '*.rs' | head -5", None),
    ("false | true; echo \"ps=${PIPESTATUS[@]}\"", "ps=1 0\n"),
    ("echo $((0x10 + (1<<3)))", "24\n"),
    ("echo ~root", "/root\n"),
]

check("9 result responses", len(results) == 9, f"got {len(results)}")
typed_ok = True
ids_ok = True
status_ok = True
cwd_ok = True
for i, res in enumerate(results):
    rid = f"r{i+1}"
    if res.get("id") != rid:
        ids_ok = False
    for field, kind in (("status", int), ("stdout", str), ("cwd", str),
                        ("env_delta", dict), ("duration_ms", int)):
        if field not in res or not isinstance(res[field], kind):
            typed_ok = False
    if res.get("status") != 0 or res.get("termination") != "exited":
        status_ok = False
    if i >= 1 and res.get("cwd") != repo + "/rust":
        cwd_ok = False
check("ids correlate r1..r9", ids_ok)
check("all statuses 0 / termination exited", status_ok)
check("typed fields status/stdout/cwd/env_delta/duration_ms present", typed_ok)
check("cwd persists after 'cd rust' for r2..r9", cwd_ok)

if len(results) >= 3:
    check("pwd returns the persisted rust dir", results[2].get("stdout") == repo + "/rust\n",
          repr(results[2].get("stdout")))
if len(results) >= 1:
    check("env_delta reports x=1", results[0].get("env_delta", {}).get("x") == "1",
          str(results[0].get("env_delta")))
for idx, (_, want) in enumerate(expected):
    if want is not None and idx < len(results):
        check(f"r{idx+1} stdout == {want!r}", results[idx].get("stdout") == want,
              repr(results[idx].get("stdout")))

print("-- per-request table (duration_ms from responses, rtt_ms from wall clock) --")
print(f"{'id':<5} {'command':<46} {'status':>6} {'duration_ms':>11} {'rtt_ms':>7} {'bash_ms':>8}")
for i, (cmd, _) in enumerate(expected):
    res = results[i] if i < len(results) else {}
    shown = cmd if len(cmd) <= 46 else cmd[:43] + "..."
    print(f"{'r'+str(i+1):<5} {shown:<46} {str(res.get('status','-')):>6} "
          f"{str(res.get('duration_ms','-')):>11} {str(rtt.get('r'+str(i+1), '-')):>7} "
          f"{str(bt.get('r'+str(i+1), '-')):>8}")

print()
print("-- verification --")
fails = 0
for name, ok, detail in checks:
    print(f"[{'PASS' if ok else 'FAIL'}] {name}" + (f"  {detail}" if detail and not ok else ""))
    fails += not ok

print()
print("-- comparison --")
print(f"processes:      session=1   bash={len(expected)} (+1 extra = {len(expected)+1})")
print(f"wall_ms:        session={session_ms}   bash9={bash_ms}")
print(f"output_bytes:   session_json={sbytes}   bash_raw={bbytes}")
if int(bash_ms) > 0:
    print(f"wall ratio:     session/bash9 = {int(session_ms)/int(bash_ms):.2f}x "
          f"({'slower' if int(session_ms) > int(bash_ms) else 'faster'})")
print(f"verification:   {len(checks)-fails}/{len(checks)} passed")
sys.exit(1 if fails else 0)
PY
VERIFY_RC=$?

if [[ -s $TMP/stderr ]]; then
    printf '\n-- agent stderr (non-empty) --\n'
    cat "$TMP/stderr"
fi
printf '\nRESULT: verification_rc=%s agent_exit=%s\n' "$VERIFY_RC" "$AGENT_EXIT"
exit "$VERIFY_RC"
