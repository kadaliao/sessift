#!/usr/bin/env python3
"""Generate fictional sessions for README screenshots: two machines, three sources."""

import json
import os
import shutil
import sys
import time
import uuid
from datetime import datetime, timezone

ROOT = sys.argv[1]
BIN = sys.argv[2]
NOW = time.time()


def iso(t):
    return datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def uid(seed):
    return str(uuid.uuid5(uuid.NAMESPACE_URL, "sessift-demo/" + seed))


# (machine, source, project, minutes ago, turns)
# turns: ("u", text) | ("a", text) | ("t", tool, input, output)
S = []


def session(machine, source, project, ago, *turns):
    S.append((machine, source, project, ago, turns))


session("local", "claude", "shop-api", 4,
    ("u", "结算接口在高峰期偶尔返回 502，帮我查一下原因"),
    ("a", "先看一下网关的上游超时配置和最近的错误日志。"),
    ("t", "Bash", {"command": "grep -n proxy_read_timeout deploy/nginx.conf", "description": "查看上游超时"}, "12:    proxy_read_timeout 5s;"),
    ("a", "原因找到了：上游超时只有 **5 秒**，而结算服务在高峰期 p99 超过 6 秒。\n\n建议分两步：\n\n1. 把 `proxy_read_timeout` 调到 15 秒，先止血\n2. 给库存查询加缓存，把 p99 压回 2 秒以内\n\n```nginx\nlocation /checkout {\n    proxy_read_timeout 15s;\n}\n```"),
    ("u", "改吧，然后灰度发布"),
    ("t", "Edit", {"file_path": "deploy/nginx.conf", "old_string": "proxy_read_timeout 5s;", "new_string": "proxy_read_timeout 15s;"}, "The file deploy/nginx.conf has been updated."),
    ("a", "已修改。发布计划：\n\n- 先灰度 10% 流量，观察 30 分钟错误率\n- 502 比例低于 0.1% 再全量\n- 有问题用 `make rollback` 一键回滚到上一个版本"),
)
session("local", "claude", "notes-app", 31,
    ("u", "两台设备同时编辑同一篇笔记时，同步冲突怎么合并？"),
    ("a", "推荐按段落做三方合并：以上次同步的版本为基准，两边都改了同一段时保留两份，并在界面上提示用户选择。\n\n- 只有一边改动：直接采用\n- 两边改了不同段落：自动合并\n- 改了同一段：生成冲突副本"),
)
session("local", "codex", "infra", 125,
    ("u", "staging 的数据库迁移 0042 失败了，看看 runbook，现在还能不能回滚？"),
    ("a", "我先读一下回滚手册和迁移日志。"),
    ("t", "exec_command", {"cmd": "sed -n '1,40p' runbooks/rollback.md && tail -n 5 logs/migrate-0042.log"}, "# Rollback\n1. Pause writers\n2. Restore the latest snapshot\nERROR: canceling statement due to lock timeout"),
    ("a", "迁移 0042 卡在 backfill 的锁等待上，数据还没有被改写，**可以回滚**。\n\n步骤：\n\n1. 暂停写入：`infractl pause --env staging`\n2. 恢复 01:30 的快照\n3. 重新跑迁移，把 backfill 拆成每批 5000 行\n\n快照在 02:00 夜间清理时才会过期，在那之前回滚都来得及。"),
    ("u", "先暂停写入，把状态给我看看"),
    ("t", "exec_command", {"cmd": "infractl pause --env staging && infractl status --env staging"}, "writers paused (3 workers)\nstatus: paused, replication lag 0s"),
    ("a", "写入已暂停（3 个 worker），复制延迟 0 秒。你确认后我就开始恢复快照。"),
)
session("local", "pi", "notes-app", 190,
    ("u", "Add a dark mode toggle and persist the choice in localStorage"),
    ("a", "I'll add a `useTheme` hook that reads the saved value on load and writes it back on change, then wire the toggle into the settings page."),
    ("t", "bash", {"command": "rg -n 'theme' src/settings"}, "src/settings/Appearance.tsx:14:  // TODO theme"),
    ("a", "Done. The toggle lives in **Settings → Appearance** and falls back to the system preference when nothing is saved."),
)
session("local", "claude", "infra", 660,
    ("u", "terraform plan 显示要重建 RDS 实例，帮我看看为什么"),
    ("a", "是 `engine_version` 从 15.4 变成了 15.6 引起的。小版本升级不需要重建，加上 `allow_major_version_upgrade = false` 并设置 `apply_immediately = false`，plan 就会变成原地更新。"),
)
session("local", "claude", "blog", 1500,
    ("u", "给博客加上 RSS 订阅和站点地图"),
    ("a", "用 `@astrojs/rss` 生成 `/rss.xml`，用 `@astrojs/sitemap` 生成站点地图，两者都在构建时输出，不需要额外的服务。"),
)
session("local", "claude", "payments", 2900,
    ("u", "退款回调会重复触发，怎么做幂等？"),
    ("a", "用回调里的 `refund_id` 做幂等键：\n\n1. 在同一个事务里先插入幂等记录，再改订单状态\n2. 唯一键冲突说明已经处理过，直接返回 200\n3. 任何一步失败就回滚事务，让支付方重试"),
)
session("local", "codex", "weather-cli", 5800,
    ("u", "Cache forecast responses for 10 minutes so repeated calls don't hit the API"),
    ("a", "Added a small on-disk cache keyed by city and units, with a 10 minute TTL. `--no-cache` skips it."),
)
session("local", "claude", "docs-site", 8600,
    ("u", "把文档站从 Docusaurus 迁到 Astro，保留现有的 URL"),
    ("a", "可以。用 Astro 的内容集合承载现有的 Markdown，路由按原来的目录结构生成；旧的 `/docs/next/` 前缀加一条重定向。"),
)
session("local", "claude", "k8s-config", 13000,
    ("u", "给 api 的 Deployment 加上 readiness probe 和滚动更新策略"),
    ("a", "加了 `/healthz` 的 readiness probe，滚动更新用 `maxUnavailable: 0`、`maxSurge: 25%`。新版本有问题时用 `kubectl rollout undo deploy/api` 回滚。"),
)
session("local", "claude", "weather-cli", 26000,
    ("u", "Add a --json flag that prints the raw forecast"),
    ("a", "Added `--json`; it prints the API response as-is so it can be piped into `jq`."),
)
session("local", "claude", "blog", 29000,
    ("u", "评论区接入 giscus，跟随站点的深色模式"),
    ("a", "giscus 支持通过 `postMessage` 切换主题，我在主题切换时同步发一条消息过去。"),
)
session("local", "codex", "shop-api", 17500,
    ("u", "订单表加索引之前，评估一下锁表风险"),
    ("a", "用 `CREATE INDEX CONCURRENTLY`，不会阻塞写入；代价是建索引时间翻倍，而且不能放在事务里。"),
)
session("local", "claude", "notes-app", 4300,
    ("u", "笔记全文搜索太慢了，五千篇就要两秒"),
    ("a", "现在是每次搜索都遍历全部笔记。换成 SQLite FTS5 建全文索引后，五千篇的查询在 10 毫秒以内。"),
)

session("devbox", "claude", "ml-train", 24,
    ("u", "训练到第 3 个 epoch 时 loss 变成 NaN，帮我排查"),
    ("a", "多半是学习率预热结束后梯度爆炸。先打开梯度裁剪（`max_norm=1.0`），再检查数据里有没有空标签。"),
    ("t", "Bash", {"command": "python check_labels.py data/train.jsonl", "description": "检查空标签"}, "empty labels: 0\nmax grad norm (epoch 3): 4821.7"),
    ("a", "数据没问题，梯度范数在第 3 个 epoch 冲到 4800。加上梯度裁剪后重跑，loss 已经稳定下降。"),
)
session("devbox", "codex", "crawler", 300,
    ("u", "爬虫被目标站点限流了，加上指数退避和代理池"),
    ("a", "加了指数退避（1s 起，最多 60s，带随机抖动），遇到 429 时换下一个代理；代理连续失败 3 次就暂时摘掉。"),
)
session("devbox", "claude", "data-pipeline", 1450,
    ("u", "每日 ETL 失败后要能自动重跑，失败的那天要能回滚到上一个分区"),
    ("a", "方案：\n\n- 每个分区先写到临时目录，校验行数后再原子地切换\n- 失败时保留上一个分区不动，相当于自动回滚\n- 重跑交给调度器，最多 3 次，间隔 15 分钟"),
)
session("devbox", "pi", "infra", 4400,
    ("u", "写一个发布脚本：先备份，再上传，失败自动回滚"),
    ("a", "脚本先把线上目录打包备份，再用 rsync 上传；任何一步出错就用备份恢复，并退出非零状态码。"),
    ("t", "bash", {"command": "bash -n scripts/release.sh"}, ""),
    ("a", "语法检查通过。加了 `--dry-run` 方便先演练一遍。"),
)
session("devbox", "pi", "crawler", 2800,
    ("u", "把抓到的页面存成 parquet，按天分区"),
    ("a", "用 pyarrow 按 `dt=YYYY-MM-DD` 分区写入，每个文件控制在 128MB 左右。"),
)
session("devbox", "claude", "ml-train", 7300,
    ("u", "把训练脚本改成支持多卡 DDP"),
    ("a", "改用 `torchrun` 启动，模型包一层 `DistributedDataParallel`，采样器换成 `DistributedSampler`，只在 rank 0 保存检查点和写日志。"),
)
session("devbox", "codex", "payments", 10100,
    ("u", "Add retries with idempotency keys to the payout client"),
    ("a", "Each payout now carries an `Idempotency-Key`; retries reuse it, so a retried request can never pay twice."),
)
session("devbox", "codex", "docs-site", 18900,
    ("u", "生成 API 参考页，按模块分组"),
    ("a", "从 OpenAPI 文件生成参考页，每个 tag 一页，左侧目录按模块分组。"),
)


def claude(home, sid, cwd, lines):
    proj = cwd.replace("/", "-")
    path = os.path.join(home, ".claude/projects", proj, sid + ".jsonl")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    out, parent = [], None
    for i, (t, kind, data) in enumerate(lines):
        u = uid(f"{sid}/{i}")
        base = {"parentUuid": parent, "isSidechain": False, "userType": "external", "cwd": cwd,
                "sessionId": sid, "version": "2.1.314", "gitBranch": "main", "uuid": u, "timestamp": iso(t)}
        if kind == "u":
            base.update(type="user", message={"role": "user", "content": data})
        elif kind == "a":
            base.update(type="assistant", message={"role": "assistant", "model": "claude-opus-5-5",
                                                   "content": [{"type": "text", "text": data}]})
        elif kind == "call":
            name, args, tid = data
            base.update(type="assistant", message={"role": "assistant", "model": "claude-opus-5-5",
                        "content": [{"type": "tool_use", "id": tid, "name": name, "input": args}]})
        elif kind == "result":
            output, tid = data
            base.update(type="user", message={"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tid, "content": output}]})
        out.append(json.dumps(base, ensure_ascii=False))
        parent = u
    open(path, "w").write("\n".join(out) + "\n")


def codex(home, sid, cwd, lines):
    t0 = lines[0][0]
    day = datetime.fromtimestamp(t0, timezone.utc)
    path = os.path.join(home, ".codex/sessions", day.strftime("%Y/%m/%d"),
                        f"rollout-{day.strftime('%Y-%m-%dT%H-%M-%S')}-{sid}.jsonl")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    out = [{"timestamp": iso(t0), "type": "session_meta", "payload": {
        "id": sid, "session_id": sid, "timestamp": iso(t0), "cwd": cwd,
        "originator": "codex_cli_rs", "cli_version": "0.164.0", "source": "cli"}}]
    for t, kind, data in lines:
        if kind == "u":
            p = {"type": "message", "role": "user", "content": [{"type": "input_text", "text": data}]}
        elif kind == "a":
            p = {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": data}]}
        elif kind == "call":
            name, args, cid = data
            p = {"type": "function_call", "name": name, "arguments": json.dumps(args, ensure_ascii=False), "call_id": cid}
        else:
            output, cid = data
            p = {"type": "function_call_output", "call_id": cid, "output": output}
        out.append({"timestamp": iso(t), "type": "response_item", "payload": p})
    open(path, "w").write("\n".join(json.dumps(x, ensure_ascii=False) for x in out) + "\n")


def pi(home, sid, cwd, lines):
    t0 = lines[0][0]
    stamp = datetime.fromtimestamp(t0, timezone.utc).strftime("%Y-%m-%dT%H-%M-%S-000Z")
    d = os.path.join(home, ".pi/agent/sessions", "-" + cwd.replace("/", "-") + "--")
    os.makedirs(d, exist_ok=True)
    out = [{"type": "session", "version": 3, "id": sid, "timestamp": iso(t0), "cwd": cwd}]
    parent = None
    for i, (t, kind, data) in enumerate(lines):
        if kind == "u":
            m = {"role": "user", "content": [{"type": "text", "text": data}]}
        elif kind == "a":
            m = {"role": "assistant", "content": [{"type": "text", "text": data}]}
        elif kind == "call":
            name, args, cid = data
            m = {"role": "assistant", "content": [{"type": "toolCall", "id": cid, "name": name, "arguments": args}]}
        else:
            output, cid = data
            m = {"role": "toolResult", "toolCallId": cid, "toolName": "bash",
                 "content": [{"type": "text", "text": output}], "isError": False}
        m["timestamp"] = int(t * 1000)
        mid = f"m{i:07d}"
        out.append({"type": "message", "id": mid, "parentId": parent, "timestamp": iso(t), "message": m})
        parent = mid
    open(os.path.join(d, f"{stamp}_{sid}.jsonl"), "w").write(
        "\n".join(json.dumps(x, ensure_ascii=False) for x in out) + "\n")


shutil.rmtree(ROOT, ignore_errors=True)
for n, (machine, source, project, ago, turns) in enumerate(S):
    home = os.path.join(ROOT, machine, "home")
    base = "/Users/alice/work" if machine == "local" else "/home/alice/src"
    cwd = f"{base}/{project}"
    sid = uid(f"session/{n}")
    if source == "codex":
        sid = "019d" + sid[4:]
    # Expand turns into timed lines that end `ago` minutes ago.
    flat = []
    for turn in turns:
        if turn[0] == "t":
            _, name, args, output = turn
            cid = ("toolu_" if source == "claude" else "call_") + uid(f"{sid}/{len(flat)}")[:12]
            flat += [("call", (name, args, cid)), ("result", (output, cid))]
        else:
            flat.append(turn)
    end = NOW - ago * 60
    lines = [(end - (len(flat) - 1 - i) * 40, kind, data) for i, (kind, data) in enumerate(flat)]
    {"claude": claude, "codex": codex, "pi": pi}[source](home, sid, cwd, lines)

for machine in ("local", "devbox"):
    os.makedirs(os.path.join(ROOT, machine, "sessift"), exist_ok=True)
open(os.path.join(ROOT, "devbox/sessift/config.toml"), "w").write('[backend]\nkind = "native"\n')
far = ["env", f"HOME={ROOT}/devbox/home", f"SESSIFT_HOME={ROOT}/devbox/sessift", BIN, "serve", "--stdio"]
open(os.path.join(ROOT, "local/sessift/config.toml"), "w").write(
    '[defaults]\nmachines = ["local", "devbox"]\ntimeout = "10s"\n\n[backend]\nkind = "native"\n\n'
    f'[[machine]]\nid = "devbox"\ncommand = {json.dumps(far)}\n')
print(f"{len(S)} sessions")
