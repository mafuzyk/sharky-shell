#!/usr/bin/env bash
# agent-dogfood-bench.sh
#
# Phase F5 dogfooding benchmarks for native Agent Code Mode. Measures real
# coding-agent workloads: dynamic fan-out over hostile filenames, noisy log
# triage with output projection and on-demand capture paging, capability
# discovery / isolation / secret hygiene, and truthful failure auditing.
#
# Usage: bash scripts/agent-dogfood-bench.sh
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

if [[ ! -x $SHARKY_BIN ]]; then
    printf 'FATAL: SHARKY_BIN=%s is missing or not executable (run cargo build first)\n' "$SHARKY_BIN" >&2
    exit 2
fi

export SHARKY_BIN REPO
exec "$PYTHON" - <<'PY'
import json
import os
import platform
import shutil
import subprocess
import tempfile
import time

SHARKY_BIN = os.environ["SHARKY_BIN"]
REPO = os.environ["REPO"]


def fd_count(pid: int) -> int:
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return -1


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


class Agent:
    def __init__(self, cwd: str, extra_args=()):
        self.proc = subprocess.Popen(
            [SHARKY_BIN, "agent", "--stdio", "--no-config", *extra_args],
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

    def call(self, obj: dict) -> dict:
        obj = {"schema_version": 1, **obj}
        self.send(obj)
        return self.recv()

    def close(self) -> int:
        self.send({"schema_version": 1, "id": "close-bench", "op": "close"})
        ack = self.recv()
        closed = self.recv()
        assert ack.get("type") == "ack", ack
        assert closed.get("type") == "closed", closed
        return self.proc.wait(timeout=10)


def make_hostile_files(root: str) -> dict:
    """Create 24 files with shell-hostile names; return {name: expected_lines}."""
    names = [
        "plain.txt",
        "a simple.txt",
        "b 'single'.txt",
        'c "double".txt',
        "d $HOME file.txt",
        "e ; semi.txt",
        "f `true` tick.txt",
        "g *glob*.txt",
        "h (paren).txt",
        "i & amp.txt",
        "j back\\slash.txt",
        "k hash#tag.txt",
        "l bang!.txt",
        "m pipe|pipe.txt",
        "n redirect>out.txt",
        "o redirect<in.txt",
        "p newline-less tab\tname.txt",
        "q equals=name.txt",
        "r colon:name.txt",
        "s brace{a,b}.txt",
        "t bracket[x].txt",
        "u tilde~name.txt",
        "v question?.txt",
        "w space   run.txt",
    ]
    expected = {}
    for index, name in enumerate(names, start=1):
        path = os.path.join(root, name)
        with open(path, "w", encoding="utf-8") as fh:
            for line in range(1, index + 1):
                fh.write(f"{name} line {line}\n")
        expected[name] = index
    return expected


def bench_dynamic_fanout(repo: str) -> dict:
    print("\n== 1. Dynamic fan-out: bash round trips vs one op:\"code\" turn ==")
    work = tempfile.mkdtemp(prefix="sharky-dogfood-fanout.")
    agent = Agent(work)
    pid = agent.proc.pid
    baseline_fds = fd_count(pid)
    try:
        files_dir = os.path.join(work, "hostile files")
        os.makedirs(files_dir)
        expected = make_hostile_files(files_dir)

        # (a) Bash-style baseline: discovery round trip + one naive, unquoted
        #     shell interpolation per file (the fragility this mode removes).
        t0 = time.monotonic()
        discovery = agent.call({
            "id": "fan-discover",
            "op": "exec",
            "argv": ["find", files_dir, "-maxdepth", "1", "-type", "f"],
        })
        rounds = 1
        assert discovery.get("status") == 0, discovery
        paths = [line for line in discovery["stdout"].splitlines() if line]
        assert len(paths) == len(expected), (len(paths), len(expected))

        quoting_failures = 0
        correct = 0
        for index, path in enumerate(paths):
            response = agent.call({
                "id": f"fan-bash-{index}",
                "op": "exec",
                "command": f"wc -l {path}",
            })
            rounds += 1
            name = os.path.basename(path)
            want = expected[name]
            got = None
            stdout = response.get("stdout", "")
            fields = stdout.strip().split()
            if fields and fields[0].isdigit():
                got = int(fields[0])
            if response.get("status") == 0 and got == want:
                correct += 1
            else:
                quoting_failures += 1
        bash_wall_ms = (time.monotonic() - t0) * 1000.0
        print(
            f"  bash baseline: rounds={rounds} wall={bash_wall_ms:8.1f} ms "
            f"correct={correct}/{len(expected)} quoting_failures={quoting_failures}"
        )

        # (b) One native code turn: typed argv, local map_parallel fan-out.
        program = r"""
            let found = run(["find", args.dir, "-maxdepth", "1", "-type", "f"]);
            let files = found.stdout.trim().lines().sort();
            let results = map_parallel(files, 8, f => ({ argv: ["wc", "-l", f] }));
            let counts = results.map(r => r.stdout.trim().split(" ")[0]);
            return { count: files.len(), counts: counts };
        """
        t0 = time.monotonic()
        response = agent.call({
            "id": "fan-code",
            "op": "code",
            "code": program,
            "args": {"dir": files_dir},
        })
        code_wall_ms = (time.monotonic() - t0) * 1000.0

        assert response.get("type") == "code", response
        assert response.get("status") == 0, response
        value = response["value"]
        counts = sorted(int(c) for c in value["counts"])
        assert value["count"] == len(expected), value
        assert counts == list(range(1, len(expected) + 1)), counts
        audit = response["audit"]
        assert audit["calls"] == len(expected) + 1, audit
        assert audit["failed_calls"] == 0, audit
        print(
            f"  op:\"code\":    rounds=1 wall={code_wall_ms:8.1f} ms "
            f"correct={value['count']}/{len(expected)} audit_calls={audit['calls']} "
            f"failed_calls={audit['failed_calls']}"
        )
        print(
            f"  delta: rounds {rounds} -> 1, wall {bash_wall_ms:.1f} -> {code_wall_ms:.1f} ms "
            f"({bash_wall_ms / max(code_wall_ms, 0.001):.2f}x), "
            f"quoting failures {quoting_failures} -> 0"
        )
        assert quoting_failures >= 1, "hostile filename set failed to expose unquoted interpolation"
        assert code_wall_ms < bash_wall_ms, (code_wall_ms, bash_wall_ms)

        after_fds = fd_count(pid)
        assert after_fds == baseline_fds, f"FD leak: {baseline_fds} -> {after_fds}"
        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"
        return {
            "bash_rounds": rounds,
            "bash_wall_ms": bash_wall_ms,
            "bash_correct": correct,
            "quoting_failures": quoting_failures,
            "code_rounds": 1,
            "code_wall_ms": code_wall_ms,
            "code_correct": value["count"],
            "files": len(expected),
        }
    finally:
        shutil.rmtree(work, ignore_errors=True)


def write_noisy_log(path: str) -> int:
    """Write a ~5000-line ANSI log with a FAILED marker every 500 lines."""
    total = 0
    with open(path, "w", encoding="utf-8") as fh:
        for i in range(1, 5001):
            if i % 500 == 0:
                fh.write(f"\x1b[31mFAILED test_case_{i:05d}: assertion mismatch\x1b[0m\n")
            elif i % 250 == 0:
                fh.write(f"\x1b[33mwarn {i:05d}: retrying shard\x1b[0m\n")
            else:
                fh.write(f"\x1b[32mok\x1b[0m {i:05d} parsed module_{i:05d} in {i % 97} ms\n")
            total += 1
    return total


def bench_noisy_log(repo: str) -> dict:
    print("\n== 2. Noisy log triage: projection + capture paging + FD stability ==")
    work = tempfile.mkdtemp(prefix="sharky-dogfood-log.")
    log_path = os.path.join(work, "build.log")
    line_total = write_noisy_log(log_path)
    raw_bytes = os.path.getsize(log_path)
    agent = Agent(repo)
    pid = agent.proc.pid
    baseline_fds = fd_count(pid)
    baseline_rss = read_rss_kb(pid)
    try:
        spec = {
            "strip_ansi": True,
            "grep": "FAILED",
            "context": 1,
            "tail_lines": 20,
            "max_bytes": 16384,
        }
        t0 = time.monotonic()
        response = agent.call({
            "id": "log-projected",
            "op": "exec",
            "argv": ["cat", log_path],
            "output": spec,
        })
        wall_ms = (time.monotonic() - t0) * 1000.0
        assert response.get("type") == "result", response
        assert response.get("status") == 0, response
        assert response.get("stdout_bytes") == raw_bytes, (
            response.get("stdout_bytes"),
            raw_bytes,
        )
        projected = response["stdout"]
        delivered_bytes = len(json.dumps(projected).encode("utf-8"))
        projection = response["projection"]
        assert projection["stdout_total_lines"] == line_total, projection
        assert projection["stdout_matched_lines"] == 10, projection
        assert "FAILED" in projected, projected[:200]
        assert "\x1b" not in projected
        capture_id = response["capture_id"]
        assert capture_id.startswith("cap-"), capture_id
        print(
            f"  file={line_total} lines / {raw_bytes} raw bytes; projected wall={wall_ms:.1f} ms"
        )
        print(
            f"  wire: raw_stdout_bytes={response['stdout_bytes']} "
            f"delivered_json_stdout_bytes={delivered_bytes} "
            f"ratio={response['stdout_bytes'] / max(delivered_bytes, 1):.1f}x "
            f"returned_lines={projection['stdout_returned_lines']}/"
            f"{projection['stdout_total_lines']} matched={projection['stdout_matched_lines']}"
        )

        windows = [(1, 20), (2500, 20), (line_total - 19, 20)]
        window_bytes = []
        for start, count in windows:
            capture = agent.call({
                "id": f"log-read-{start}",
                "op": "read_capture",
                "capture_id": capture_id,
                "stream": "stdout",
                "line_start": start,
                "line_count": count,
            })
            assert capture.get("type") == "capture", capture
            assert capture.get("encoding") == "utf8", capture
            data_bytes = len(capture["data"].encode("utf-8"))
            window_bytes.append(data_bytes)
            print(
                f"  read_capture line_start={start:4d} count={count:2d} "
                f"bytes={data_bytes:5d} returned={capture.get('returned_lines', '?')} "
                f"total_lines={capture.get('total_lines', '?')}"
            )

        # FD stability across repeated projected executions and paged reads.
        repeats = 30
        t0 = time.monotonic()
        for i in range(repeats):
            repeat = agent.call({
                "id": f"log-repeat-{i}",
                "op": "exec",
                "argv": ["cat", log_path],
                "output": {"strip_ansi": True, "tail_lines": 5, "max_bytes": 2048},
            })
            assert repeat.get("status") == 0, repeat
        for i in range(3):
            again = agent.call({
                "id": f"log-repeat-read-{i}",
                "op": "read_capture",
                "capture_id": capture_id,
                "stream": "stdout",
                "line_start": 1,
                "line_count": 10,
            })
            assert again.get("type") == "capture", again
        repeat_ms = (time.monotonic() - t0) * 1000.0
        after_fds = fd_count(pid)
        peak_rss = read_rss_kb(pid)
        print(
            f"  FD stability: {baseline_fds} -> {after_fds} over {repeats} projected execs "
            f"+ 3 read_capture in {repeat_ms:.1f} ms; RSS {baseline_rss} -> {peak_rss} KiB"
        )
        assert after_fds == baseline_fds, f"FD leak: {baseline_fds} -> {after_fds}"

        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"
        return {
            "raw_bytes": raw_bytes,
            "delivered_bytes": delivered_bytes,
            "lines": line_total,
            "wall_ms": wall_ms,
            "window_bytes": window_bytes,
            "fds": (baseline_fds, after_fds),
        }
    finally:
        shutil.rmtree(work, ignore_errors=True)


def bench_discovery_and_hygiene(repo: str) -> dict:
    print("\n== 3. Discovery, isolation and secret hygiene ==")
    work = tempfile.mkdtemp(prefix="sharky-dogfood-iso.")
    try:
        caps = subprocess.run(
            [SHARKY_BIN, "capabilities", "--json"],
            cwd=REPO,
            text=True,
            capture_output=True,
            check=True,
        )
        assert caps.stderr == "", caps.stderr
        caps_json = json.loads(caps.stdout)

        agent = Agent(repo)
        try:
            ready_protocol = agent.ready["protocol"]
            caps_protocol = caps_json["protocol"]
            assert caps_json["schema_version"] == 1, caps_json
            assert caps_json["shell_version"], caps_json
            assert caps_protocol["ops"] == ready_protocol["ops"], "ops drifted"
            assert caps_protocol["features"] == ready_protocol["features"], "features drifted"
            assert caps_protocol["limits"] == ready_protocol["limits"], "limits drifted"
            print(
                f"  capabilities --json: shell={caps_json['shell_version']} "
                f"ops={len(caps_protocol['ops'])} features={len(caps_protocol['features'])} "
                f"matches ready.protocol (ops/features/limits)"
            )

            # Isolated exec must not mutate or exit the shared session.
            setup = agent.call({
                "id": "iso-setup",
                "op": "exec",
                "command": f"cd '{work}' && export EXPORTED_VAR=parent && (exit 7)",
            })
            assert setup.get("status") == 7, setup
            isolated = agent.call({
                "id": "iso-run",
                "op": "exec",
                "isolated": True,
                "command": "cd /; export LEAK=from_iso EXPORTED_VAR=mutated; exit 9",
            })
            assert isolated.get("status") == 9, isolated
            assert isolated.get("cwd") == "/", isolated
            state = agent.call({
                "id": "iso-state",
                "op": "state",
                "include": ["env"],
            })
            assert state.get("cwd") == work, state
            assert state.get("last_status") == 7, state
            env = state["env"]
            assert env.get("EXPORTED_VAR") == "parent", env.get("EXPORTED_VAR")
            assert "LEAK" not in env, env.get("LEAK")
            print(
                "  isolated exec: child status=9 cwd=/ but parent cwd/env/last_status "
                "unchanged (cwd preserved, EXPORTED_VAR=parent, no LEAK, last_status=7)"
            )

            # Non-isolated exec does mutate the shared session, as designed.
            mutated = agent.call({
                "id": "iso-mutate",
                "op": "exec",
                "command": "cd /; export NONISO=1",
            })
            assert mutated.get("status") == 0, mutated
            after = agent.call({
                "id": "iso-after",
                "op": "state",
                "include": ["env"],
            })
            assert after.get("cwd") == "/", after
            assert after["env"].get("NONISO") == "1", after["env"].get("NONISO")
            print("  non-isolated exec: parent cwd -> / and NONISO=1 (expected mutation)")
        finally:
            rc = agent.close()
            assert rc == 0, f"non-zero agent exit: {rc}"

        # Redaction session.
        redacted = Agent(repo, extra_args=["--redact-env"])
        try:
            set_res = redacted.call({
                "id": "redact-set",
                "op": "exec",
                "command": "export MY_API_KEY=k_456 DB_PASSWORD=pw OK_VAL=visible",
            })
            assert set_res.get("status") == 0, set_res
            delta = set_res["env_delta"]
            assert delta.get("MY_API_KEY") == "<redacted>", delta
            assert delta.get("DB_PASSWORD") == "<redacted>", delta
            assert delta.get("OK_VAL") == "visible", delta
            state = redacted.call({
                "id": "redact-state",
                "op": "state",
                "include": ["env"],
            })
            env = state["env"]
            assert env.get("MY_API_KEY") == "<redacted>", env.get("MY_API_KEY")
            assert env.get("DB_PASSWORD") == "<redacted>", env.get("DB_PASSWORD")
            assert env.get("OK_VAL") == "visible", env.get("OK_VAL")
            iso_delta_res = redacted.call({
                "id": "redact-iso",
                "op": "exec",
                "isolated": True,
                "command": "export DB_PASSWORD=pw2 OK_VAL=yes",
            })
            iso_delta = iso_delta_res["env_delta"]
            assert iso_delta.get("DB_PASSWORD") == "<redacted>", iso_delta
            assert iso_delta.get("OK_VAL") == "yes", iso_delta
            print(
                "  --redact-env: MY_API_KEY/DB_PASSWORD -> <redacted> in env_delta, "
                "state.env and isolated env_delta; OK_VAL stays visible"
            )
        finally:
            rc = redacted.close()
            assert rc == 0, f"non-zero agent exit: {rc}"

        return {"ops": len(caps_protocol["ops"]), "features": len(caps_protocol["features"])}
    finally:
        shutil.rmtree(work, ignore_errors=True)


def bench_truthful_audit(repo: str) -> dict:
    print("\n== 4. Truthful code audit (failures cannot be hidden) ==")
    agent = Agent(repo)
    try:
        program = r"""
            let a = run(["false"]);
            let b = sh("(exit 7)");
            let c = run(["sh", "-c", "exit 9"]);
            return { ok: true, statuses: [a.status, b.status, c.status] };
        """
        response = agent.call({
            "id": "audit-code",
            "op": "code",
            "code": program,
        })
        assert response.get("type") == "code", response
        assert response.get("status") == 0, response
        assert response["value"]["ok"] is True, response["value"]
        assert response["value"]["statuses"] == [1, 7, 9], response["value"]
        audit = response["audit"]
        assert audit["calls"] == 3, audit
        assert audit["failed_calls"] == 3, audit
        assert audit["first_failure_status"] == 1, audit
        print(
            f"  value claims ok=true while audit reports calls={audit['calls']} "
            f"failed_calls={audit['failed_calls']} "
            f"first_failure_status={audit['first_failure_status']} "
            f"statuses={response['value']['statuses']}"
        )
        rc = agent.close()
        assert rc == 0, f"non-zero agent exit: {rc}"
        return {"calls": audit["calls"], "failed_calls": audit["failed_calls"]}
    finally:
        pass


def main() -> None:
    print(f"binary: {SHARKY_BIN}")
    print(f"repo:   {REPO}")
    print(
        f"host:   {platform.node()} {platform.system()} {platform.release()} "
        f"{platform.machine()} python={platform.python_version()}"
    )
    started = time.monotonic()
    fanout = bench_dynamic_fanout(REPO)
    noisy = bench_noisy_log(REPO)
    hygiene = bench_discovery_and_hygiene(REPO)
    audit = bench_truthful_audit(REPO)
    total = time.monotonic() - started
    print("\n== Summary (observed) ==")
    print(
        f"  fan-out: {fanout['files']} files, bash rounds={fanout['bash_rounds']} "
        f"wall={fanout['bash_wall_ms']:.1f} ms correct={fanout['bash_correct']} "
        f"quoting_failures={fanout['quoting_failures']} | code rounds=1 "
        f"wall={fanout['code_wall_ms']:.1f} ms correct={fanout['code_correct']}"
    )
    print(
        f"  logs:    {noisy['lines']} lines raw={noisy['raw_bytes']} B "
        f"delivered={noisy['delivered_bytes']} B "
        f"windows={noisy['window_bytes']} B fds={noisy['fds'][0]}->{noisy['fds'][1]}"
    )
    print(f"  discovery: ops={hygiene['ops']} features={hygiene['features']} consistent")
    print(f"  audit:    calls={audit['calls']} failed={audit['failed_calls']} truthful")
    print(f"  total wall: {total:.1f} s")


if __name__ == "__main__":
    main()
PY
