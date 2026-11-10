#!/usr/bin/env python3
"""Concurrent sessions against the playground: how start time and memory grow with the number of
sessions running at once.

  scripts/playground.sh up
  scripts/bench-sessions.py [counts...]        default: 1 2 4 8

Each session is `claudette -p` on its own project, scripted by the mock API: a Read, a Bash call that
runs for a few seconds (so the sessions overlap), and a reply. While they run, memory is sampled every
200 ms on both machines; claude's own processes are counted apart from claudette's.
"""
import json, os, shlex, shutil, statistics, subprocess, sys, tempfile, threading, time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PLAYGROUND = ROOT / "scripts/playground.sh"
BASH_SECONDS = 3


def env():
    out = subprocess.run([str(PLAYGROUND), "env", "mock"], capture_output=True, text=True, check=True).stdout
    e = dict(os.environ)
    for line in out.splitlines():
        words = shlex.split(line)
        if words[:1] == ["export"]:
            e.update(w.split("=", 1) for w in words[1:])
        elif words[:1] == ["alias"]:
            e["CLAUDETTE_BIN"] = words[1].split("=", 1)[1]
    return e


def remote(cmd):
    return subprocess.run([str(PLAYGROUND), "ssh", cmd], capture_output=True, text=True).stdout


def scenario():
    return [
        {"tool": "Read", "input": {"file_path": "{dir}/README.md"}},
        {"tool": "Bash", "input": {"command": f"sleep {BASH_SECONDS} && echo done"}},
        {"text": "{{last_tool_result}}"},
    ]


def register(sid, steps):
    subprocess.run([str(PLAYGROUND), "scenario", sid, json.dumps(steps)], capture_output=True, check=True)


def local_rss():
    """KB of claudette processes on this machine (the playground's ssh excluded)."""
    out = subprocess.run(["ps", "-axo", "rss=,args="], capture_output=True, text=True).stdout
    return sum(int(l.split(None, 1)[0]) for l in out.splitlines() if "claudette-" in l and "bench-sessions" not in l and "playground.sh" not in l)


REMOTE_PS = "ps -eo rss=,comm=,args= | awk '{ if ($2==\"claude\") c+=$1; else if ($0 ~ /__helper|sshfs/) h+=$1 } END { print c+0, h+0 }'"


def run(n, e):
    base = Path(tempfile.mkdtemp(prefix="bench-"))
    procs, times = [], []
    for i in range(n):
        d = base / f"p{i}"
        d.mkdir()
        (d / "README.md").write_text(f"project {i}\n")
        subprocess.run(["git", "init", "-q"], cwd=d)
        sid = f"bench{n}x{i}{int(time.time())}"
        steps = json.loads(json.dumps(scenario()).replace("{dir}", str(d)))
        register(sid, steps)
        procs.append((d, sid))
    peak = {"local": 0, "claude": 0, "helper": 0}
    stop = threading.Event()

    def sample():
        while not stop.is_set():
            peak["local"] = max(peak["local"], local_rss())
            c, h = (int(x) for x in (remote(REMOTE_PS).split() or ["0", "0"]))
            peak["claude"] = max(peak["claude"], c)
            peak["helper"] = max(peak["helper"], h)
            time.sleep(0.2)

    sampler = threading.Thread(target=sample, daemon=True)
    sampler.start()
    started = time.time()

    def one(d, sid):
        t = time.time()
        r = subprocess.run([e["CLAUDETTE_BIN"], "-p", f"[[scenario:{sid}]] go", "--allowedTools", "Bash", "--output-format", "json"], cwd=d, env=e, capture_output=True, text=True)
        ok = r.returncode == 0 and json.loads(r.stdout or "{}").get("result") == "done"
        times.append((time.time() - t, ok, r.stderr.strip()[-200:]))

    threads = [threading.Thread(target=one, args=p) for p in procs]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    total = time.time() - started
    stop.set()
    sampler.join()
    shutil.rmtree(base)
    walls = [t for t, _, _ in times]
    failed = [err for _, ok, err in times if not ok]
    return {
        "sessions": n,
        "ok": n - len(failed),
        "median_s": round(statistics.median(walls), 2),
        "max_s": round(max(walls), 2),
        "overhead_s": round(statistics.median(walls) - BASH_SECONDS, 2),
        "total_s": round(total, 2),
        "local_mb": round(peak["local"] / 1024, 1),
        "remote_claude_mb": round(peak["claude"] / 1024, 1),
        "remote_claudette_mb": round(peak["helper"] / 1024, 1),
        "errors": failed[:2],
    }


def main():
    counts = [int(a) for a in sys.argv[1:]] or [1, 2, 4, 8]
    e = env()
    rows = [run(n, e) for n in counts]
    keys = ["sessions", "ok", "median_s", "max_s", "overhead_s", "total_s", "local_mb", "remote_claude_mb", "remote_claudette_mb"]
    print("| " + " | ".join(keys) + " |")
    print("|" + "---|" * len(keys))
    for r in rows:
        print("| " + " | ".join(str(r[k]) for k in keys) + " |")
    for r in rows:
        for err in r["errors"]:
            print(f"{r['sessions']} sessions, failure: {err}", file=sys.stderr)


if __name__ == "__main__":
    main()
