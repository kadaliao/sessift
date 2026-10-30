#!/usr/bin/env python3
"""Search quality evaluation over a private query set.

Usage: scripts/eval.py [--sessift PATH] [--dir DIR] [--env K=V ...] [--prefix] [--save NAME]
                       [--compare NAME] [-v]

The query set holds real session content, so it lives outside the repository, in
~/.sessift/eval/ by default:

  known.jsonl    {"q": "...", "session_id": "...", "source": "...", "paraphrase": false}
                 a query and the session it should find ("skip": true lines are ignored)
  topics.jsonl   {"q": "..."}  broad queries; reports how many automatic threads
                 (guardian, exec) and subagents reach the top 10
  exclude.txt    one substring of a transcript path per line; matching sessions are dropped
                 from results (e.g. the session that wrote the queries, which contains them)
  runs/NAME.json saved results for --compare

Metrics, on the local machine with one result per session (as the interactive view shows):
MRR@10 and found@10 for the known-item queries (paraphrase queries reported separately),
noise in the top 10 of the topic queries, and the median latency of the command.
--env passes settings to sessift (e.g. --env SESSIFT_RANK=kind=0), to compare variants on
the same index.
"""

import argparse
import json
import os
import sqlite3
import statistics
import subprocess
import sys
import time
from pathlib import Path

DEPTH = 50


def load_jsonl(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def search(args, env, query):
    cmd = [args.sessift, "search", *query.split(), "-m", "local", "-n", str(DEPTH),
           "--per-session", "--format", "jsonl"] + (["--prefix"] if args.prefix else [])
    start = time.perf_counter()
    out = subprocess.run(cmd, capture_output=True, text=True, env=env)
    elapsed = (time.perf_counter() - start) * 1000
    if out.returncode not in (0, 1):
        sys.exit(f"sessift failed on {query!r}: {out.stderr.strip()}")
    hits = [json.loads(line) for line in out.stdout.splitlines() if line.strip()]
    return hits, elapsed


def kinds_lookup(home):
    db = home / "index" / "meta.sqlite"
    if not db.exists():
        return lambda hit: None
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)

    def kind(hit):
        row = conn.execute(
            "SELECT kind FROM sessions WHERE session_id = ? AND path = ?",
            (hit["session_id"], hit["source_path"]),
        ).fetchone()
        return row[0] if row else None

    return kind


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--sessift", default="sessift")
    parser.add_argument("--dir", default=os.path.expanduser("~/.sessift/eval"))
    parser.add_argument("--env", action="append", default=[], metavar="K=V")
    parser.add_argument("--save", metavar="NAME")
    parser.add_argument("--compare", metavar="NAME")
    parser.add_argument("--prefix", action="store_true",
                        help="search as the interactive view does while typing")
    parser.add_argument("-v", "--verbose", action="store_true",
                        help="list known-item queries that miss the top 10")
    args = parser.parse_args()

    eval_dir = Path(args.dir)
    env = dict(os.environ)
    for pair in args.env:
        key, _, value = pair.partition("=")
        env[key] = value
    home = Path(env.get("SESSIFT_HOME", os.path.expanduser("~/.sessift")))
    exclude = [line.strip() for line in (eval_dir / "exclude.txt").read_text().splitlines()
               if line.strip()] if (eval_dir / "exclude.txt").exists() else []
    kind_of = kinds_lookup(home)

    def keep(hit):
        return not any(part in hit["source_path"] for part in exclude)

    # Warm up (first exec of a new binary is slow on macOS).
    search(args, env, "warmup")

    latencies = []
    known = []
    for item in load_jsonl(eval_dir / "known.jsonl"):
        if item.get("skip"):
            continue
        hits, ms = search(args, env, item["q"])
        latencies.append(ms)
        hits = [h for h in hits if keep(h)]
        rank = next((i + 1 for i, h in enumerate(hits)
                     if h["session_id"] == item["session_id"]), None)
        known.append({"q": item["q"], "session_id": item["session_id"],
                      "paraphrase": bool(item.get("paraphrase")), "rank": rank,
                      "results": len(hits)})

    topics = []
    for item in load_jsonl(eval_dir / "topics.jsonl"):
        hits, ms = search(args, env, item["q"])
        latencies.append(ms)
        top = [h for h in hits if keep(h)][:10]
        kinds = [kind_of(h) or "main" for h in top]
        topics.append({"q": item["q"], "auto": sum(k in ("guardian", "exec") for k in kinds),
                       "subagent": sum(k == "subagent" for k in kinds),
                       "tool_roles": sum(h["role"] in ("tool_use", "tool_result") for h in top)})

    def summarize(rows):
        if not rows:
            return {}
        return {
            "queries": len(rows),
            "mrr10": round(sum(1 / r["rank"] for r in rows if r["rank"] and r["rank"] <= 10)
                           / len(rows), 3),
            "found1": sum(1 for r in rows if r["rank"] == 1),
            "found10": sum(1 for r in rows if r["rank"] and r["rank"] <= 10),
            "found50": sum(1 for r in rows if r["rank"]),
        }

    result = {
        "known": summarize([r for r in known if not r["paraphrase"]]),
        "paraphrase": summarize([r for r in known if r["paraphrase"]]),
        "topics": {
            "queries": len(topics),
            "auto_in_top10": sum(t["auto"] for t in topics),
            "subagent_in_top10": sum(t["subagent"] for t in topics),
            "tool_role_hits_in_top10": sum(t["tool_roles"] for t in topics),
        },
        "latency_ms": {
            "p50": round(statistics.median(latencies), 1) if latencies else None,
            "p90": round(sorted(latencies)[int(len(latencies) * 0.9)], 1) if latencies else None,
        },
        "env": args.env,
        "per_query": {"known": known, "topics": topics},
    }

    def show(name, value, before=None):
        diff = ""
        if isinstance(value, (int, float)) and isinstance(before, (int, float)) and value != before:
            diff = f"  ({value - before:+.3g})"
        print(f"  {name:28} {value}{diff}")

    previous = None
    if args.compare:
        previous = json.loads((eval_dir / "runs" / f"{args.compare}.json").read_text())
    for section in ("known", "paraphrase", "topics", "latency_ms"):
        print(section)
        for key, value in result[section].items():
            show(key, value, previous and previous.get(section, {}).get(key))

    if previous:
        before = {(r["q"], r["session_id"]): r["rank"] for r in previous["per_query"]["known"]}
        moved = []
        for r in known:
            old = before.get((r["q"], r["session_id"]), "n/a")
            if old != r["rank"]:
                moved.append((old, r["rank"], r["q"]))
        if moved:
            print(f"rank changes ({len(moved)}):")
            for old, new, q in moved:
                print(f"  {str(old):>4} -> {str(new):<4} {q}" if args.verbose
                      else f"  {str(old):>4} -> {str(new)}")
    if args.verbose:
        print("known-item queries outside the top 10:")
        for r in known:
            if not r["rank"] or r["rank"] > 10:
                tag = " (paraphrase)" if r["paraphrase"] else ""
                print(f"  rank {str(r['rank']):>4}  of {r['results']:>2}  {r['q']}{tag}")

    if args.save:
        runs = eval_dir / "runs"
        runs.mkdir(exist_ok=True)
        (runs / f"{args.save}.json").write_text(json.dumps(result, ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main()
