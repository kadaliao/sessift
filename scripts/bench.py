#!/usr/bin/env python3
"""Compare sessift and memex wall-clock latency on the same queries.

Usage: scripts/bench.py [--runs N] [--sessift PATH] [--remote ID] [--query Q]
Every command's stdout goes to /dev/null; a non-zero exit is reported as a failure.
"""

import argparse
import statistics
import subprocess
import time


def measure(cmd, runs):
    times, failures = [], 0
    for _ in range(runs):
        start = time.perf_counter()
        proc = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        times.append((time.perf_counter() - start) * 1000)
        if proc.returncode != 0:
            failures += 1
    times.sort()
    p90 = times[min(len(times) - 1, int(len(times) * 0.9))]
    return statistics.median(times), p90, times[0], failures


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--sessift", default="sessift")
    parser.add_argument("--memex", default="memex")
    parser.add_argument("--remote", default="mm")
    parser.add_argument("--query", default="部署")
    args = parser.parse_args()

    memex = [args.memex, "--no-update-check", "--non-interactive"]
    sessift = [args.sessift]
    r, q = args.remote, args.query
    cases = [
        ("search, local + remote", memex + ["search", q, "--limit", "20"], sessift + ["search", q, "-n", "20"]),
        ("search, remote only", memex + ["search", q, "--limit", "20", "--machine", r], sessift + ["search", q, "-n", "20", "-m", r]),
        ("search, local only", memex + ["search", q, "--limit", "20", "--machine", "local"], sessift + ["search", q, "-n", "20", "-m", "local"]),
        ("50 sessions, local + remote", memex + ["sessions", "--limit", "50"], sessift + ["sessions", "-n", "50"]),
        ("search, remote, no broker", None, sessift + ["--no-broker", "search", q, "-n", "20", "-m", r]),
    ]
    # Warm both tools (broker start, index refresh) before measuring.
    for _, a, b in cases:
        for cmd in (a, b):
            if cmd:
                subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    print(f"{'case':30} {'tool':8} {'median':>8} {'p90':>8} {'min':>8}  fails")
    for name, memex_cmd, sessift_cmd in cases:
        for tool, cmd in (("memex", memex_cmd), ("sessift", sessift_cmd)):
            if cmd is None:
                continue
            median, p90, low, fails = measure(cmd, args.runs)
            print(f"{name:30} {tool:8} {median:7.0f}ms {p90:7.0f}ms {low:7.0f}ms  {fails}")


if __name__ == "__main__":
    main()
