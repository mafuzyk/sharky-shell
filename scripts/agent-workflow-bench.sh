#!/usr/bin/env bash
# agent-workflow-bench.sh
#
# Measures `op:"workflow"` scheduling overhead, 100-node DAG execution, peak RSS,
# file-descriptor stability and zombie hygiene (plus optional Cargo target-dir
# lock vs isolated target-dir comparison with `--cargo`).
#
# Usage: bash scripts/agent-workflow-bench.sh [--cargo]
# Override binary/repo via SHARKY_BIN / REPO / PYTHON environment variables.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO=${REPO:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
if [[ -z ${SHARKY_BIN:-} ]]; then
    if [[ -x $REPO/rust/target/release/sharky ]]; then
        SHARKY_BIN="$REPO/rust/target/release/sharky"
    else
        SHARKY_BIN="$REPO/rust/target/debug/sharky"
    fi
fi
PYTHON=${PYTHON:-python3}
RUN_CARGO=0
if [[ ${1:-} == "--cargo" ]]; then
    RUN_CARGO=1
fi

if [[ ! -x $SHARKY_BIN ]]; then
    printf 'FATAL: SHARKY_BIN=%s is missing or not executable (run cargo build first)\n' "$SHARKY_BIN" >&2
    exit 2
fi

export SHARKY_BIN REPO RUN_CARGO
exec "$PYTHON" - <<'PY'
import json
import os
import shutil
import subprocess
import tempfile
import time

SHARKY_BIN = os.environ["SHARKY_BIN"]
REPO = os.environ["REPO"]
RUN_CARGO = os.environ.get("RUN_CARGO") == "1"


def read_rss_kb(pid: int) -> int:
    try:
        with open(f"/proc/{pid}/status", "r", encoding="utf-8") as fh:
            for line in fh:
                if line.startswith("VmHWM:") or line.startswith("VmRSS:"):
                    parts = line.split()
                    if len(parts) >= 2:
                        return int(parts[1])
    except OSError:
        pass
    return 0


def fd_count(pid: int) -> int:
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return -1


def zombie_count(ppid: int) -> int:
    zombies = 0
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            with open(f"/proc/{name}/stat", "r", encoding="utf-8") as fh:
                stat = fh.read()
            rparen = stat.rfind(")")
            fields = stat[rparen + 2 :].split()
            if len(fields) >= 2 and fields[0] == "Z" and int(fields[1]) == ppid:
                zombies += 1
        except (OSError, ValueError):
            continue
    return zombies


class Agent:
    def __init__(self, cwd: str):
        self.proc = subprocess.Popen(
            [SHARKY_BIN, "agent", "--stdio", "--no-config"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=cwd,
            text=True,
            close_fds=True,
        )
        self.ready = self.recv()
        assert self.ready.get("type") == "ready", f"unexpected greeting: {self.ready}"

    def send(self, obj: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def recv(self) -> dict:
        assert self.proc.stdout is not None
        line = self.proc.stdout.readline()
        if not line:
            stderr = self.proc.stderr.read() if self.proc.stderr else ""
            raise RuntimeError(f"unexpected EOF from agent (stderr={stderr!r})")
        return json.loads(line)

    def close(self) -> int:
        self.send({"schema_version": 1, "id": "close-bench", "op": "close"})
        ack = self.recv()
        closed = self.recv()
        assert ack.get("type") == "ack", ack
        assert closed.get("type") == "closed", closed
        return self.proc.wait(timeout=5)


def run_bench() -> None:
    print(f"binary: {SHARKY_BIN}")
    agent = Agent(REPO)
    pid = agent.proc.pid
    baseline_fds = fd_count(pid)

    # 1. Scaling & scheduler overhead over 32 empty/true tasks at concurrency 1, 8, 32.
    print("\n== Concurrency scaling (32 x `true`) ==")
    for conc in (1, 8, 32):
        tasks = [{"id": f"t{i}", "command": "true"} for i in range(32)]
        t0 = time.monotonic()
        agent.send({
            "schema_version": 1,
            "id": f"scale-{conc}",
            "op": "workflow",
            "max_concurrency": conc,
            "tasks": tasks,
        })
        resp = agent.recv()
        wall_ms = (time.monotonic() - t0) * 1000.0
        sum_task_ms = sum(t["duration_us"] for t in resp["tasks"]) / 1000.0
        overhead_ms = max(0.0, resp["duration_ms"] - (sum_task_ms / conc))
        assert resp["status"] == 0, resp
        print(
            f"  concurrency={conc:2d}  wall={wall_ms:6.1f} ms  "
            f"group={resp['duration_ms']:4d} ms  sum_nodes={sum_task_ms:6.1f} ms  "
            f"est_overhead_over_ideal={overhead_ms:5.1f} ms"
        )

    # 2. 100-node DAG (4 chains of 24 + fan-in + 3 leaves), concurrency 8.
    print("\n== 100-node DAG (`max_concurrency: 8`) ==")
    dag_tasks = []
    tails = []
    for chain in range(4):
        for step in range(24):
            tid = f"c{chain}_{step}"
            entry = {"id": tid, "command": "true"}
            if step > 0:
                entry["depends_on"] = [f"c{chain}_{step - 1}"]
            dag_tasks.append(entry)
            if step == 23:
                tails.append(tid)
    dag_tasks.append({"id": "summary", "command": "true", "depends_on": tails})
    for i in range(3):
        dag_tasks.append({"id": f"leaf_{i}", "command": "true", "depends_on": ["summary"]})

    t0 = time.monotonic()
    agent.send({
        "schema_version": 1,
        "id": "dag-100",
        "op": "workflow",
        "max_concurrency": 8,
        "tasks": dag_tasks,
    })
    dag = agent.recv()
    wall_ms = (time.monotonic() - t0) * 1000.0
    after_fds = fd_count(pid)
    zombies = zombie_count(pid)
    peak_kb = read_rss_kb(pid)
    assert dag["status"] == 0, dag
    print(
        f"  nodes={len(dag['tasks'])} started={dag['started_tasks']} status={dag['status']} "
        f"wall={wall_ms:.1f} ms  peak_rss={peak_kb} KiB  "
        f"fds={baseline_fds}->{after_fds}  zombies={zombies}"
    )
    assert after_fds == baseline_fds, f"FD leak: {baseline_fds} -> {after_fds}"
    assert zombies == 0, f"zombies left: {zombies}"

    if RUN_CARGO:
        print("\n== Optional Cargo measurement (locked shared target vs separate CARGO_TARGET_DIR) ==")
        tmp = tempfile.mkdtemp(prefix="sharky-cargo-bench.")
        try:
            crate_dir = os.path.join(tmp, "mini")
            os.makedirs(os.path.join(crate_dir, "src"))
            with open(os.path.join(crate_dir, "Cargo.toml"), "w", encoding="utf-8") as fh:
                fh.write('[package]\nname = "mini"\nversion = "0.1.0"\nedition = "2021"\n')
            with open(os.path.join(crate_dir, "src", "lib.rs"), "w", encoding="utf-8") as fh:
                fh.write("pub fn add(a: i32, b: i32) -> i32 { a + b }\n#[test]\nfn it_works() { assert_eq!(add(2, 2), 4); }\n")

            shared_target = os.path.join(tmp, "target-shared")
            agent.send({
                "schema_version": 1,
                "id": "cargo-locked",
                "op": "workflow",
                "max_concurrency": 3,
                "tasks": [
                    {"id": "check", "cwd": crate_dir, "lock": "cargo-target", "env": {"CARGO_TARGET_DIR": shared_target}, "command": "cargo check -j 2 -q"},
                    {"id": "test", "cwd": crate_dir, "lock": "cargo-target", "env": {"CARGO_TARGET_DIR": shared_target}, "command": "cargo test -j 2 -q"},
                    {"id": "doc", "cwd": crate_dir, "lock": "cargo-target", "env": {"CARGO_TARGET_DIR": shared_target}, "command": "cargo doc -j 2 -q --no-deps"},
                ],
            })
            r_locked = agent.recv()

            agent.send({
                "schema_version": 1,
                "id": "cargo-split",
                "op": "workflow",
                "max_concurrency": 3,
                "tasks": [
                    {"id": "check", "cwd": crate_dir, "env": {"CARGO_TARGET_DIR": os.path.join(tmp, "t-check")}, "command": "cargo check -j 2 -q"},
                    {"id": "test", "cwd": crate_dir, "env": {"CARGO_TARGET_DIR": os.path.join(tmp, "t-test")}, "command": "cargo test -j 2 -q"},
                    {"id": "doc", "cwd": crate_dir, "env": {"CARGO_TARGET_DIR": os.path.join(tmp, "t-doc")}, "command": "cargo doc -j 2 -q --no-deps"},
                ],
            })
            r_split = agent.recv()
            print(f"  locked shared target:   duration={r_locked['duration_ms']} ms status={r_locked['status']}")
            print(f"  separate target dirs:   duration={r_split['duration_ms']} ms status={r_split['status']}")
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    rc = agent.close()
    assert rc == 0, f"non-zero exit: {rc}"


if __name__ == "__main__":
    run_bench()
PY
