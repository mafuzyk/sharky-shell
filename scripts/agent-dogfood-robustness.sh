#!/usr/bin/env bash
# agent-dogfood-robustness.sh
#
# Roadmap section 13.2 dogfooding scenarios 7-12. Measures real robustness
# signals from a running `sharky agent --stdio` session: cancellation and
# process-group cleanup, bounded wire output for unbounded/binary producers,
# adversarial shell parity against bash, continuity after failures, stdin
# framing and monorepo cwd/env isolation. Also audits descriptor and zombie
# stability. Uses an already-built binary; never builds or tests the Rust tree.
#
# Usage: SHARKY_BIN=/path/to/release/sharky bash scripts/agent-dogfood-robustness.sh
# Override REPO / PYTHON via the environment as in the other agent benches.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO=${REPO:-$(cd -- "$SCRIPT_DIR/.." && pwd)}
SHARKY_BIN=${SHARKY_BIN:-$REPO/rust/target/release/sharky}
PYTHON=${PYTHON:-python3}

if [[ ! -x $SHARKY_BIN ]]; then
    printf 'FATAL: SHARKY_BIN=%s is missing or not executable\n' "$SHARKY_BIN" >&2
    exit 2
fi

export SHARKY_BIN REPO
exec "$PYTHON" - <<'PY'
import base64
import json
import os
import platform
import shutil
import subprocess
import tempfile
import time

SHARKY_BIN = os.environ["SHARKY_BIN"]
REPO = os.environ["REPO"]
SEQ = [0]


def next_id(prefix: str) -> str:
    SEQ[0] += 1
    return f"{prefix}-{SEQ[0]}"


def fd_count(pid: int) -> int:
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return -1


def zombie_count(pid: int) -> int:
    """Zombie children of the agent process, not unrelated host zombies."""
    total = 0
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", "r", encoding="utf-8") as fh:
                fields = fh.read().rsplit(")", 1)[1].split()
            total += fields[0] == "Z" and int(fields[1]) == pid
        except (OSError, IndexError, ValueError):
            pass
    return total


def pgid_survivors(pgid: int) -> list:
    found = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", "r", encoding="utf-8") as fh:
                fields = fh.read().rsplit(")", 1)[1].split()
            if int(fields[2]) == pgid:
                found.append(entry)
        except (OSError, ValueError, IndexError):
            pass
    return found


class Agent:
    def __init__(self, cwd: str):
        self.proc = subprocess.Popen(
            [SHARKY_BIN, "agent", "--stdio", "--no-config"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=cwd,
            text=True,
            bufsize=1,
            close_fds=True,
        )
        self.ready = self.recv()
        assert self.ready.get("type") == "ready", f"unexpected greeting: {self.ready}"

    def send(self, obj: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps({"schema_version": 1, **obj}) + "\n")
        self.proc.stdin.flush()

    def recv(self) -> dict:
        assert self.proc.stdout is not None
        line = self.proc.stdout.readline()
        if not line:
            stderr = self.proc.stderr.read() if self.proc.stderr else ""
            raise RuntimeError(f"unexpected EOF from agent (stderr={stderr!r})")
        return json.loads(line)

    def call(self, obj: dict) -> dict:
        self.send(obj)
        return self.recv()

    def close(self) -> int:
        self.send({"id": next_id("close"), "op": "close"})
        ack = self.recv()
        closed = self.recv()
        assert ack.get("type") == "ack", ack
        assert closed.get("type") == "closed", closed
        return self.proc.wait(timeout=10)


def stream_bytes(value: dict, prefix: str) -> int:
    text = value.get(prefix, "")
    if value.get(f"{prefix}_encoding") == "base64":
        return len(base64.b64decode(text))
    return len(text.encode("utf-8"))


def bench_cancellation(repo: str) -> dict:
    print("\n== 7. Stuck command and cancellation / unbounded producer ==")
    agent = Agent(repo)
    fds_start = fd_count(agent.proc.pid)
    zombies_start = zombie_count(agent.proc.pid)
    try:
        agent.send({
            "id": "cancel-target",
            "op": "exec",
            "command": r"""sh -c 'trap "" TERM; sleep 30'""",
        })
        time.sleep(0.15)
        started = time.monotonic()
        ack = agent.call({
            "id": "cancel-op",
            "op": "cancel",
            "target": "cancel-target",
            "grace_ms": 200,
        })
        result = agent.recv()
        wall_ms = (time.monotonic() - started) * 1000.0
        assert ack.get("type") == "ack" and ack.get("outcome") == "delivered", ack
        assert result.get("type") == "result" and result.get("id") == "cancel-target", result
        assert result.get("termination") == "cancelled", result
        assert result.get("status") == 143, result
        assert wall_ms < 3000.0, wall_ms
        pgid = result.get("pgid")
        assert isinstance(pgid, int), result
        survivors = pgid_survivors(pgid)
        assert not survivors, f"surviving pgid {pgid}: {survivors}"
        print(
            f"  (a) TERM-ignoring child: status={result['status']} "
            f"termination={result['termination']} grace_ms=200 "
            f"wall={wall_ms:.1f} ms pgid={pgid} survivors={len(survivors)}"
        )

        started = time.monotonic()
        producer = agent.call({
            "id": "unbounded-yes",
            "op": "exec",
            "argv": ["yes", "SHARKY"],
            "output": {"tail_lines": 20, "max_bytes": 4096},
            "timeout_ms": 1200,
        })
        wall_ms = (time.monotonic() - started) * 1000.0
        assert producer.get("type") == "result", producer
        assert producer.get("termination") == "timeout" and producer.get("status") == 124, producer
        delivered = stream_bytes(producer, "stdout")
        raw = producer.get("stdout_bytes", 0)
        assert producer.get("stdout_truncated") is True, producer
        assert delivered <= 4096, delivered
        assert raw > delivered * 4, (raw, delivered)
        projection = producer.get("projection", {})
        assert projection.get("stdout_returned_lines") == 20, projection
        print(
            f"  (b) unbounded yes: raw_stdout_bytes={raw} delivered_bytes={delivered} "
            f"stdout_truncated={producer['stdout_truncated']} wall={wall_ms:.1f} ms "
            f"termination={producer['termination']} returned_lines="
            f"{projection.get('stdout_returned_lines')} total_lines={projection.get('stdout_total_lines')}"
        )

        fds_end = fd_count(agent.proc.pid)
        zombies_end = zombie_count(agent.proc.pid)
        assert fds_end == fds_start, f"FD leak: {fds_start} -> {fds_end}"
        assert zombies_end == 0, f"agent zombies: {zombies_start} -> {zombies_end}"
        print(
            f"  resource audit: fds={fds_start}->{fds_end} "
            f"zombies={zombies_start}->{zombies_end}"
        )
        return {
            "cancel_ms": wall_ms,
            "raw": raw,
            "delivered": delivered,
            "fds": (fds_start, fds_end),
            "zombies": (zombies_start, zombies_end),
        }
    finally:
        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"


def bench_binary_output(repo: str) -> dict:
    print("\n== 8. Huge and binary output / projected text log ==")
    work = tempfile.mkdtemp(prefix="sharky-robust-binary.")
    agent = Agent(repo)
    fds_start = fd_count(agent.proc.pid)
    zombies_start = zombie_count(agent.proc.pid)
    try:
        blob = os.path.join(work, "blob.bin")
        raw_blob = bytes(range(256)) + b"payload\n" * 3
        with open(blob, "wb") as fh:
            while fh.tell() < 20 * 1024 * 1024:
                fh.write(raw_blob)
        blob_bytes = os.path.getsize(blob)
        binary = agent.call({
            "id": "binary-cat",
            "op": "exec",
            "argv": ["cat", blob],
            "output": {"max_bytes": 4096},
        })
        assert binary.get("type") == "result", binary
        assert binary.get("status") == 0, binary
        assert binary.get("stdout_encoding") == "base64", binary
        delivered = stream_bytes(binary, "stdout")
        raw = binary.get("stdout_bytes", 0)
        assert raw == blob_bytes, (raw, blob_bytes)
        assert binary.get("stdout_truncated") is True, binary
        assert delivered <= 4096, delivered
        print(
            f"  binary: file={blob_bytes} B raw_stdout_bytes={raw} "
            f"delivered_bytes={delivered} encoding={binary['stdout_encoding']} "
            f"truncated={binary['stdout_truncated']}"
        )

        log = os.path.join(work, "build.log")
        line_total = 30000
        with open(log, "w", encoding="utf-8") as fh:
            for i in range(line_total):
                if i % 100 == 0:
                    fh.write(f"2026-11-21 ERROR FAILED module_{i % 97} trace id={i}\n")
                else:
                    fh.write(
                        f"2026-11-21 INFO module_{i % 97} parsed "
                        f"module_{i % 97} in {i % 101} ms\n"
                    )
        log_bytes = os.path.getsize(log)
        projected = agent.call({
            "id": "log-projected",
            "op": "exec",
            "argv": ["cat", log],
            "output": {"grep": "FAILED", "tail_lines": 5, "max_bytes": 4096},
        })
        assert projected.get("type") == "result" and projected.get("status") == 0, projected
        p_raw = projected.get("stdout_bytes", 0)
        p_delivered = stream_bytes(projected, "stdout")
        proj = projected.get("projection", {})
        assert p_raw == log_bytes, (p_raw, log_bytes)
        assert p_delivered <= 4096, p_delivered
        assert proj.get("stdout_total_lines") == line_total, proj
        assert proj.get("stdout_matched_lines") == 300, proj
        assert proj.get("stdout_returned_lines") == 5, proj
        assert "FAILED" in projected.get("stdout", ""), projected.get("stdout", "")[:200]
        print(
            f"  text log: lines={line_total} raw_stdout_bytes={p_raw} "
            f"delivered_bytes={p_delivered} matched={proj.get('stdout_matched_lines')} "
            f"returned={proj.get('stdout_returned_lines')}"
        )

        fds_end = fd_count(agent.proc.pid)
        zombies_end = zombie_count(agent.proc.pid)
        assert fds_end == fds_start, f"FD leak: {fds_start} -> {fds_end}"
        assert zombies_end == 0, f"agent zombies: {zombies_start} -> {zombies_end}"
        print(
            f"  resource audit: fds={fds_start}->{fds_end} "
            f"zombies={zombies_start}->{zombies_end}"
        )
        return {
            "blob_raw": raw,
            "blob_delivered": delivered,
            "log_raw": p_raw,
            "log_delivered": p_delivered,
            "fds": (fds_start, fds_end),
            "zombies": (zombies_start, zombies_end),
        }
    finally:
        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"
        shutil.rmtree(work, ignore_errors=True)


def bench_shell_parity(repo: str) -> dict:
    print("\n== 9. Adversarial shell errors vs bash ==")
    work = tempfile.mkdtemp(prefix="sharky-robust-parity.")
    with open(os.path.join(work, "exists"), "w", encoding="utf-8") as fh:
        fh.write("old\n")
    with open(os.path.join(work, "src"), "w", encoding="utf-8") as fh:
        fh.write("return 7\n")
    with open(os.path.join(work, "input"), "w", encoding="utf-8") as fh:
        fh.write("fd3\n")
    cases = [
        (
            "noclobber",
            "set -o noclobber; printf new > exists; "
            'printf "changed:%s" "$(cat exists)"',
        ),
        ("readonly", "readonly x=one; x=two; printf '%s' \"$x\""),
        (
            "PIPESTATUS",
            "printf 'a\\nb\\n' | grep b; printf '%s' \"${PIPESTATUS[*]}\"",
        ),
        ("source/return", 'source src; printf "status=%s" "$?"'),
        ("errexit in $()", "set -e; x=$(false); printf after"),
        ("|&", "printf 'e\\n' >&2 |& cat"),
        (">&-", "printf 'o\\n' >&-; printf 'ok\\n'"),
        ("<&-", "cat <&-; printf 'ok\\n'"),
        ("<&3 / 3<file", "exec 3<input; cat <&3; exec 3<&-; printf closed"),
        (">&3", "exec 3>out; printf hi >&3; exec 3>&-; printf 'v=%s' \"$(cat out)\""),
    ]
    agent = Agent(repo)
    fds_start = fd_count(agent.proc.pid)
    zombies_start = zombie_count(agent.proc.pid)
    checks = []
    try:
        for index, (name, code) in enumerate(cases):
            baseline = subprocess.run(
                ["bash", "-c", code], cwd=work, text=True, capture_output=True
            )
            observed = agent.call({
                "id": f"parity-{index}",
                "op": "exec",
                "command": code,
                "cwd": work,
                "isolated": True,
            })
            assert observed.get("type") == "result", observed
            checks.append(
                (name, baseline.returncode, observed.get("status"),
                 baseline.stdout, observed.get("stdout"))
            )
            assert baseline.returncode == observed.get("status"), checks[-1]
            assert baseline.stdout == observed.get("stdout"), checks[-1]
        fds_end = fd_count(agent.proc.pid)
        zombies_end = zombie_count(agent.proc.pid)
        assert fds_end == fds_start, f"FD leak: {fds_start} -> {fds_end}"
        assert zombies_end == 0, f"agent zombies: {zombies_start} -> {zombies_end}"
        for name, b_status, s_status, b_out, s_out in checks:
            print(
                f"  {name:<15} bash={b_status}/{b_out!r} "
                f"sharky={s_status}/{s_out!r} equal"
            )
        print(
            f"  cases={len(checks)} fds={fds_start}->{fds_end} "
            f"zombies={zombies_start}->{zombies_end}"
        )
        return {"cases": len(checks), "fds": (fds_start, fds_end),
                "zombies": (zombies_start, zombies_end)}
    finally:
        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"
        shutil.rmtree(work, ignore_errors=True)


def bench_continuity(repo: str) -> dict:
    print("\n== 10. Continuity after failures ==")
    agent = Agent(repo)
    fds_start = fd_count(agent.proc.pid)
    zombies_start = zombie_count(agent.proc.pid)
    failures = [
        ("parse-error", "if", 2),
        ("unknown-command", "no-such-command-sharky-xyz", 127),
        ("exit-1", "false", 1),
        ("return-outside", "return 7", 2),
    ]
    try:
        for ident, command, want in failures:
            result = agent.call({"id": ident, "op": "exec", "command": command})
            assert result.get("type") == "result", result
            assert result.get("status") == want, (ident, result.get("status"), want)
            print(
                f"  {ident}: status={result['status']} "
                f"termination={result.get('termination')}"
            )
        after = agent.call({
            "id": "continues",
            "op": "exec",
            "command": "printf 'alive:%s' \"$((21 * 2))\"",
        })
        assert after.get("type") == "result" and after.get("status") == 0, after
        assert after.get("stdout") == "alive:42", after
        state = agent.call({
            "id": "state-after",
            "op": "state",
            "include": ["env", "variables", "functions", "aliases", "traps", "jobs"],
        })
        assert state.get("type") == "state", state
        for section in ["env", "variables", "functions", "aliases", "traps", "jobs"]:
            assert section in state, (section, state)
        print(
            f"  next exec: status={after['status']} stdout={after['stdout']!r}; "
            f"state sections={len(state)} including all 6 requested"
        )
        fds_end = fd_count(agent.proc.pid)
        zombies_end = zombie_count(agent.proc.pid)
        assert fds_end == fds_start, f"FD leak: {fds_start} -> {fds_end}"
        assert zombies_end == 0, f"agent zombies: {zombies_start} -> {zombies_end}"
        print(
            f"  resource audit: fds={fds_start}->{fds_end} "
            f"zombies={zombies_start}->{zombies_end}"
        )
        return {"failures": len(failures), "fds": (fds_start, fds_end),
                "zombies": (zombies_start, zombies_end)}
    finally:
        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"


def bench_stdin(repo: str) -> dict:
    print("\n== 11. Stdin tasks and JSONL framing ==")
    agent = Agent(repo)
    fds_start = fd_count(agent.proc.pid)
    zombies_start = zombie_count(agent.proc.pid)
    payload = "alpha\nbeta\n\x00gamma\n"
    try:
        consumed = agent.call({
            "id": "stdin-cat",
            "op": "exec",
            "argv": ["cat"],
            "stdin": payload,
        })
        assert consumed.get("type") == "result" and consumed.get("status") == 0, consumed
        assert consumed.get("stdout") == payload, consumed
        assert consumed.get("stdout_bytes") == len(payload.encode("utf-8")), consumed
        ignored = agent.call({
            "id": "stdin-ignored",
            "op": "exec",
            "command": "printf ignored",
            "stdin": payload,
        })
        assert ignored.get("type") == "result" and ignored.get("status") == 0, ignored
        assert ignored.get("stdout") == "ignored", ignored
        next_request = agent.call({
            "id": "stdin-next",
            "op": "exec",
            "command": "printf framed",
        })
        assert next_request.get("type") == "result", next_request
        assert next_request.get("id") == "stdin-next", next_request
        assert next_request.get("stdout") == "framed", next_request
        print(
            f"  cat payload: bytes_in={len(payload.encode('utf-8'))} "
            f"stdout_bytes={consumed['stdout_bytes']} exact={consumed['stdout'] == payload}; "
            f"ignored-task stdout={ignored['stdout']!r}; next id={next_request['id']} "
            f"stdout={next_request['stdout']!r}"
        )
        fds_end = fd_count(agent.proc.pid)
        zombies_end = zombie_count(agent.proc.pid)
        assert fds_end == fds_start, f"FD leak: {fds_start} -> {fds_end}"
        assert zombies_end == 0, f"agent zombies: {zombies_start} -> {zombies_end}"
        print(
            f"  resource audit: fds={fds_start}->{fds_end} "
            f"zombies={zombies_start}->{zombies_end}"
        )
        return {"payload_bytes": len(payload.encode("utf-8")),
                "fds": (fds_start, fds_end), "zombies": (zombies_start, zombies_end)}
    finally:
        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"


def bench_navigation(repo: str) -> dict:
    print("\n== 12. Monorepo navigation: cwd/env isolation ==")
    work = tempfile.mkdtemp(prefix="sharky-robust-monorepo.")
    dirs = ["packages/a", "packages/b", "tools/c"]
    for rel in dirs:
        path = os.path.join(work, rel)
        os.makedirs(path)
        with open(os.path.join(path, "marker"), "w", encoding="utf-8") as fh:
            fh.write(rel + "\n")
    agent = Agent(work)
    fds_start = fd_count(agent.proc.pid)
    zombies_start = zombie_count(agent.proc.pid)
    try:
        execs = []
        for index, rel in enumerate(dirs):
            result = agent.call({
                "id": f"nav-{index}",
                "op": "exec",
                "command": 'pwd; cat marker; printf "leaf=%s\\n" "$LEAF"',
                "cwd": os.path.join(work, rel),
                "isolated": True,
                "env": {"LEAF": rel},
                "env_delta": True,
            })
            assert result.get("type") == "result", result
            assert result.get("status") == 0, result
            assert result.get("cwd") == os.path.join(work, rel), result
            assert result.get("stdout") == f"{work}/{rel}\n{rel}\nleaf={rel}\n", result
            execs.append(result)
        group = agent.call({
            "id": "nav-parallel",
            "op": "parallel",
            "policy": "wait-all",
            "max_concurrency": 3,
            "tasks": [
                {
                    "id": f"task-{index}",
                    "command": 'pwd; cat marker; printf "leaf=%s\\n" "$LEAF"',
                    "cwd": os.path.join(work, rel),
                    "env": {"LEAF": rel},
                    "env_delta": True,
                }
                for index, rel in enumerate(dirs)
            ],
        })
        assert group.get("type") == "parallel" and group.get("status") == 0, group
        tasks = group.get("tasks", [])
        assert len(tasks) == len(dirs), tasks
        for index, (task, rel) in enumerate(zip(tasks, dirs)):
            assert task.get("status") == 0 and task.get("termination") == "exited", task
            assert task.get("cwd") == os.path.join(work, rel), task
            assert task.get("id") == f"task-{index}", task
            assert task.get("stdout") == f"{work}/{rel}\n{rel}\nleaf={rel}\n", task
        parent = agent.call({
            "id": "nav-parent",
            "op": "exec",
            "command": 'pwd; printf "leaf=%s\\n" "${LEAF-unset}"',
            "isolated": True,
        })
        assert parent.get("stdout") == f"{work}\nleaf=unset\n", parent
        state = agent.call({
            "id": "nav-state",
            "op": "state",
            "include": ["env", "variables", "functions", "aliases", "traps", "jobs"],
        })
        assert state.get("cwd") == work, state
        assert "LEAF" not in state.get("env", {}), state.get("env", {})
        print(
            f"  isolated execs={len(execs)} parallel tasks={len(tasks)} "
            f"cwd_overrides={len(dirs)} all stdout matched; parent cwd={state['cwd']} "
            f"LEAF={state.get('env', {}).get('LEAF', 'unset')}"
        )
        fds_end = fd_count(agent.proc.pid)
        zombies_end = zombie_count(agent.proc.pid)
        assert fds_end == fds_start, f"FD leak: {fds_start} -> {fds_end}"
        assert zombies_end == 0, f"agent zombies: {zombies_start} -> {zombies_end}"
        print(
            f"  resource audit: fds={fds_start}->{fds_end} "
            f"zombies={zombies_start}->{zombies_end}"
        )
        return {"execs": len(execs), "tasks": len(tasks),
                "fds": (fds_start, fds_end), "zombies": (zombies_start, zombies_end)}
    finally:
        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"
        shutil.rmtree(work, ignore_errors=True)


def main() -> None:
    print(f"binary: {SHARKY_BIN}")
    print(f"repo:   {REPO}")
    print(
        f"host:   {platform.node()} {platform.system()} {platform.release()} "
        f"{platform.machine()} python={platform.python_version()}"
    )
    started = time.monotonic()
    cancellation = bench_cancellation(REPO)
    binary = bench_binary_output(REPO)
    parity = bench_shell_parity(REPO)
    continuity = bench_continuity(REPO)
    stdin = bench_stdin(REPO)
    navigation = bench_navigation(REPO)
    total = time.monotonic() - started
    print("\n== Summary (observed) ==")
    print(
        f"  7: cancel status=143/cancelled wall={cancellation['cancel_ms']:.1f} ms; "
        f"yes raw={cancellation['raw']} B delivered={cancellation['delivered']} B"
    )
    print(
        f"  8: binary raw={binary['blob_raw']} B delivered={binary['blob_delivered']} B; "
        f"log raw={binary['log_raw']} B delivered={binary['log_delivered']} B"
    )
    print(
        f"  9: {parity['cases']} bash differential cases equal status/stdout; "
        f"10: {continuity['failures']} failures then clean exec/state; "
        f"11: {stdin['payload_bytes']} stdin bytes exact, framing intact; "
        f"12: {navigation['execs']} execs + {navigation['tasks']} parallel tasks isolated"
    )
    print(f"  total wall: {total:.1f} s")
    assert total < 60.0, total


if __name__ == "__main__":
    main()
PY
