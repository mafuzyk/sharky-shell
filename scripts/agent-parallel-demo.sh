#!/usr/bin/env bash
# agent-parallel-demo.sh
#
# Manual demo (not a cargo test): one `sharky agent --stdio --no-config`
# session runs the same four read-only recon commands twice — once as a single
# `parallel` request (max_concurrency 2, policy wait-all) and once as four
# sequential `exec` requests — and validates both responses before reporting
# wall time and per-task aggregate durations.
#
# No dependencies beyond bash, python3 (JSON parsing/validation only) and core
# utilities. Run with:  bash scripts/agent-parallel-demo.sh
# Override paths with REPO / SHARKY_BIN / PYTHON / READ_TIMEOUT environment vars.
#
# The script never edits the repository: the session's cwd is the repo root and
# every command below is read-only. Build the binary first (cargo build).

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO=${REPO:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
SHARKY_BIN=${SHARKY_BIN:-$REPO/rust/target/debug/sharky}
PYTHON=${PYTHON:-python3}
READ_TIMEOUT=${READ_TIMEOUT:-10}

if [[ ! -d $REPO || ! -x $SHARKY_BIN ]]; then
    printf 'FATAL: REPO=%s SHARKY_BIN=%s (build it with cargo build)\n' "$REPO" "$SHARKY_BIN" >&2
    exit 2
fi
cd "$REPO" || exit 2

TMP=$(mktemp -d "${TMPDIR:-/tmp}/agent-parallel-demo.XXXXXX") || exit 2
AGENT_PID=
cleanup() {
    [[ -n $AGENT_PID ]] && kill -0 "$AGENT_PID" 2>/dev/null && kill "$AGENT_PID" 2>/dev/null
    exec 3>&- 2>/dev/null || :
    exec 4<&- 2>/dev/null || :
    rm -rf "$TMP"
}
trap cleanup EXIT

now_us() { local e=$EPOCHREALTIME; printf '%s' "${e//[^0-9]/}"; }
ms_between() { printf '%s' "$(( ($2 - $1) / 1000 ))"; }

# Four independent, read-only recon commands. Single quotes keep $... literal.
CMDS=(
    'git status --short --branch | head -5'
    'git diff --stat HEAD^ HEAD | head -5'
    "find rust/src -name '*.rs' | head -5"
    'wc -l rust/src/*.rs | tail -1'
)
N=${#CMDS[@]}
printf '%s\n' "${CMDS[@]}" >"$TMP/commands.txt"

json_escape() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    printf '%s' "$s"
}

printf '=== Sharky agent: one parallel batch vs %d sequential requests ===\n' "$N"
printf 'repo:   %s\n' "$REPO"
printf 'sharky: %s\n' "$SHARKY_BIN"
printf 'policy: wait-all, max_concurrency=2\n\n'

# ------------------------------------------------------------------ session ---
mkfifo "$TMP/in" "$TMP/out"
"$SHARKY_BIN" agent --stdio --no-config <"$TMP/in" >"$TMP/out" 2>"$TMP/stderr" &
AGENT_PID=$!
exec 3>"$TMP/in"
exec 4<"$TMP/out"

: >"$TMP/responses.jsonl"
: >"$TMP/times.tsv"

if IFS= read -r -t "$READ_TIMEOUT" ready <&4 && [[ -n $ready ]]; then
    printf '%s\n' "$ready" >>"$TMP/responses.jsonl"
else
    printf 'FATAL: no ready line within %ss (stderr follows)\n' "$READ_TIMEOUT" >&2
    cat "$TMP/stderr" >&2
    exit 2
fi

# -------------------------------------------------------------- parallel batch ---
tasks=
for ((i = 0; i < N; i++)); do
    esc=$(json_escape "${CMDS[$i]}")
    task=$(printf '{"id":"t%d","command":"%s"}' "$((i+1))" "$esc")
    if [[ -n $tasks ]]; then tasks+=","; fi
    tasks+=$task
done
parallel_req=$(printf '{"schema_version":1,"id":"p1","op":"parallel","policy":"wait-all","max_concurrency":2,"tasks":[%s]}' "$tasks")

PARALLEL_START=$(now_us)
printf '%s\n' "$parallel_req" >&3
if IFS= read -r -t "$READ_TIMEOUT" parallel_resp <&4 && [[ -n $parallel_resp ]]; then
    PARALLEL_END=$(now_us)
    printf '%s\n' "$parallel_resp" >>"$TMP/responses.jsonl"
else
    printf 'FATAL: no parallel response within %ss (stderr follows)\n' "$READ_TIMEOUT" >&2
    cat "$TMP/stderr" >&2
    exit 2
fi
printf 'parallel_wall_ms\t%s\n' "$(ms_between "$PARALLEL_START" "$PARALLEL_END")" >>"$TMP/times.tsv"

# ------------------------------------------------------------- sequential batch ---
SEQUENTIAL_START=$(now_us)
for ((i = 0; i < N; i++)); do
    esc=$(json_escape "${CMDS[$i]}")
    req=$(printf '{"schema_version":1,"id":"e%d","op":"exec","command":"%s"}' "$((i+1))" "$esc")
    printf '%s\n' "$req" >&3
    if IFS= read -r -t "$READ_TIMEOUT" resp <&4 && [[ -n $resp ]]; then
        printf '%s\n' "$resp" >>"$TMP/responses.jsonl"
    else
        printf 'FATAL: no exec response for e%d within %ss (stderr follows)\n' \
            "$((i+1))" "$READ_TIMEOUT" >&2
        cat "$TMP/stderr" >&2
        exit 2
    fi
done
SEQUENTIAL_END=$(now_us)
printf 'sequential_wall_ms\t%s\n' "$(ms_between "$SEQUENTIAL_START" "$SEQUENTIAL_END")" >>"$TMP/times.tsv"

# --------------------------------------------------------------- close session ---
printf '%s\n' '{"schema_version":1,"id":"close1","op":"close"}' >&3
for event in ack closed; do
    if IFS= read -r -t "$READ_TIMEOUT" line <&4 && [[ -n $line ]]; then
        printf '%s\n' "$line" >>"$TMP/responses.jsonl"
    else
        printf 'WARN: no %s line within %ss\n' "$event" "$READ_TIMEOUT" >&2
    fi
done
exec 3>&-
exec 4<&-
wait "$AGENT_PID"
AGENT_EXIT=$?
AGENT_PID=

# ------------------------------------------------- validation + measurements ---
"$PYTHON" - "$TMP/responses.jsonl" "$TMP/times.tsv" "$TMP/commands.txt" \
    "$AGENT_EXIT" <<'PY'
import json
import sys

resp_path, times_path, commands_path, agent_exit = sys.argv[1:4] + [int(sys.argv[4])]

def lines(path):
    with open(path, encoding="utf-8") as fh:
        return [ln.rstrip("\n") for ln in fh if ln.strip()]

def tsv(path):
    out = {}
    for ln in lines(path):
        parts = ln.split("\t")
        if len(parts) == 2:
            out[parts[0]] = int(parts[1])
    return out

raw = lines(resp_path)
commands = lines(commands_path)
times = tsv(times_path)
n = len(commands)
checks = []

def check(name, ok, detail=""):
    checks.append((name, bool(ok), detail))

parsed = []
ok = True
for i, line in enumerate(raw):
    try:
        parsed.append(json.loads(line))
    except Exception as exc:                       # noqa: BLE001 - report it
        ok = False
        check(f"line {i} parses as JSON", False, str(exc))
        parsed.append(None)
check("all response lines parse as JSON", ok, f"{len(raw)} lines")

ready = parsed[0] if parsed else None
ops = (ready or {}).get("protocol", {}).get("ops", [])
features = (ready or {}).get("protocol", {}).get("features", [])
check("first response is ready with schema_version 1",
      isinstance(ready, dict) and ready.get("type") == "ready"
      and ready.get("schema_version") == 1)
check("ready advertises parallel ops and features",
      "parallel" in ops and "parallel" in features, str(ops))

group = next((p for p in parsed[1:] if isinstance(p, dict)
              and p.get("type") == "parallel"), None)
check("one parallel response", group is not None and
      sum(p.get("type") == "parallel" for p in parsed[1:]
          if isinstance(p, dict)) == 1)

tasks = (group or {}).get("tasks", [])
ids = [t.get("id") for t in tasks if isinstance(t, dict)]
check(f"{n} tasks in request order t1..t{n}", ids == [f"t{i+1}" for i in range(n)], str(ids))
check("group status 0", (group or {}).get("status") == 0,
      str((group or {}).get("status")))
check("group state completed", (group or {}).get("state") == "completed",
      str((group or {}).get("state")))
check("policy wait-all and max_concurrency 2 echoed",
      (group or {}).get("policy") == "wait-all"
      and (group or {}).get("max_concurrency") == 2)
check("all tasks started", (group or {}).get("started_tasks") == n,
      str((group or {}).get("started_tasks")))
check("no output truncation", (group or {}).get("output_truncated") is False)
check("all task statuses 0 / termination exited",
      len(tasks) == n and all(t.get("status") == 0
                              and t.get("termination") == "exited"
                              for t in tasks))
check("every task stdout is non-empty",
      len(tasks) == n and all(isinstance(t.get("stdout"), str)
                              and t["stdout"].strip() for t in tasks))
check("every task reports duration_us",
      len(tasks) == n and all(isinstance(t.get("duration_us"), int)
                              for t in tasks))

results = [p for p in parsed[1:] if isinstance(p, dict) and p.get("type") == "result"]
result_ids = [r.get("id") for r in results]
check(f"{n} sequential results e1..e{n}",
      result_ids == [f"e{i+1}" for i in range(n)], str(result_ids))
check("all sequential statuses 0 / termination exited",
      len(results) == n and all(r.get("status") == 0
                                and r.get("termination") == "exited"
                                for r in results))
check("all sequential stdout is non-empty",
      len(results) == n and all(isinstance(r.get("stdout"), str)
                                and r["stdout"].strip() for r in results))
check("all sequential results report duration_us",
      len(results) == n and all(isinstance(r.get("duration_us"), int)
                                for r in results))

tail = parsed[-2:]
check("close ack then closed event",
      len(tail) == 2 and tail[0].get("type") == "ack"
      and tail[0].get("outcome") == "closed"
      and tail[1].get("type") == "closed", str(tail))
check("agent exited 0", agent_exit == 0, str(agent_exit))

print("-- per-task aggregate durations (microseconds, from the responses) --")
print(f"{'id':<4} {'command':<42} {'parallel_us':>12} {'sequential_us':>14}")
for i, command in enumerate(commands):
    p_us = tasks[i].get("duration_us") if i < len(tasks) else "-"
    s_us = results[i].get("duration_us") if i < len(results) else "-"
    shown = command if len(command) <= 42 else command[:39] + "..."
    print(f"{'t'+str(i+1):<4} {shown:<42} {str(p_us):>12} {str(s_us):>14}")

def total(items, field):
    return sum(i.get(field, 0) for i in items
               if isinstance(i.get(field, 0), int))

pw = times.get("parallel_wall_ms", "-")
sw = times.get("sequential_wall_ms", "-")
print()
print("-- wall time (one session, same four commands) --")
print(f"parallel batch wall_ms:   {pw}")
print(f"sequential batch wall_ms: {sw}")
print(f"sum of task duration_us:  parallel={total(tasks, 'duration_us')}  "
      f"sequential={total(results, 'duration_us')}")
if isinstance(pw, int) and isinstance(sw, int) and pw > 0:
    print(f"wall ratio parallel/sequential: {pw / sw:.2f}x")

print()
print("-- verification --")
fails = 0
for name, passed, detail in checks:
    print(f"[{'PASS' if passed else 'FAIL'}] {name}" + (f"  {detail}" if detail and not passed else ""))
    fails += not passed
print()
print("NOTE: parallel wins only when the tasks are genuinely independent and the")
print("machine has spare cores; shared locks (e.g. one Cargo target dir) or a")
print("saturated machine can make it slower. Tasks share the filesystem: this demo")
print("uses only read-only commands because there is no isolation or locking.")
print(f"verification: {len(checks) - fails}/{len(checks)} passed; "
      f"checks={len(checks)} fails={fails}")
sys.exit(1 if fails else 0)
PY
VERIFY_RC=$?

if [[ -s $TMP/stderr ]]; then
    printf '\n-- agent stderr (non-empty) --\n'
    cat "$TMP/stderr"
fi
printf '\nRESULT: verification_rc=%s agent_exit=%s\n' "$VERIFY_RC" "$AGENT_EXIT"
exit "$VERIFY_RC"
