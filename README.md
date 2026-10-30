# sessift

在本机和其他机器上搜索、浏览 AI 编程助手的会话记录（Claude Code、Codex、Pi、OpenCode 等）。
机器之间不同步数据：查询时到每台机器现场查，只把查好的结果传回来。

```
$ sessift search 部署 回滚 -n 3
 41.2  2026-09-20 18:29  local  claude  shop-api      assistant  7c1e9a02-…
       …灰度发布到 10% 流量，观察错误率；有问题用 `make rollback` 一键回滚 …
 33.8  2026-06-15 12:03  mm     codex   infra-notes   assistant  0b5d77f4-…
       把部署脚本、健康检查和回滚步骤整理成一页手册。
```

当前进度：远程查询层、交互界面和自研索引（阶段 2）已完成，默认后端是自研索引，不再需要 memex。路线图见 [docs/plan.md](docs/plan.md)，新会话先读 [CLAUDE.md](CLAUDE.md)。

## 性能

本机 MacBook + Mac mini（往返约 25ms、带宽 30～500KB/s），两台机器都用自研索引，命令行从启动到结束，每项 20 次取中位数（2026-10-31）：

| 场景 | memex 0.31.1 | sessift |
|---|---|---|
| 搜索，本机 + 远端 | 2171ms | 31ms |
| 搜索，只查远端 | 2198ms | 39ms |
| 搜索，只查本机 | 87ms | 22ms |
| 50 个会话，本机 + 远端 | 624ms | 31ms |
| 搜索，只查远端，不经 broker | — | 189ms |

用 `scripts/bench.py` 复现（装了 memex 时同时测它）。

自研索引（本机全量 9.7GB、47 万条记录）：

| 场景 | 耗时 |
|---|---|
| 查询前的增量检查，没有变化 | 约 19ms |
| 查询前的增量更新，有新记录（正在进行的会话） | 25～35ms |
| 冷建全量索引 | 12.9 秒，索引 482MB（memex 5GB） |

和 memex 的区别：sessift 也索引 Codex 压缩过的旧会话（`.jsonl.zst`，memex 不读）；原文件已删除的会话不保留；多个搜索词要同时出现在一条记录里（memex 是任一出现）；目前支持 Claude Code、Codex、Pi、OpenCode 四种来源。

## 安装

```sh
cargo build --release
cp target/release/sessift ~/.local/bin/    # 或其他 PATH 目录
sessift init                               # 生成 ~/.sessift/config.toml，从 ~/.memex/config.toml 导入机器
sessift remote install mm                  # 把同一个二进制装到远端 ~/.sessift/bin/sessift
sessift doctor                             # 检查连接、版本、延迟
```

远端机器需要能用 `ssh <别名>` 免密登录。每台机器第一次被查询时会在后台建索引（几秒到十几秒），之后查询前自动增量更新；也可以手动 `sessift index`。
`remote install` 要求两边是同一平台（如都是 Darwin arm64）；远端已是同一份二进制时跳过上传。

## 交互界面（Alt-M，或直接运行 `sessift`）

交互方式参考 memex 的 TUI：打字就是搜索，↓ 或 Enter 进入列表，列表里单个字母就是命令。

```
 ▌
 ▌   部署
 ▌
 匹配 42   ● local 30   ● mm 12                         m 全部机器 ▾   s 全部来源 ▾   t 全部时间 ▾
 ▌ 1mo ● claude   mm     blog            帮我把博客部署到 Vercel，用 GitHub 自动构建…
   4mo ● codex    mm     infra-notes     把部署脚本、健康检查和回滚步骤整理成一页手册。
    1d ● claude   local  shop-api        灰度发布到 10% 流量，观察错误率；有问题一键回滚。
```

| 位置 | 按键 | 作用 |
|---|---|---|
| 搜索框 | 输入 | 全文搜索（每个会话只显示最相关的一条）；为空时列出最近会话 |
| 搜索框 | ↓ / Enter | 进入列表 |
| 搜索框 | Tab | 打开右侧分栏预览并进入列表 |
| 搜索框 | Esc | 清空搜索；再按一次退出 |
| 列表 | Enter / r | 恢复会话（远端会话自动 `ssh -t`） |
| 列表 | e | 只把恢复命令放到命令行，不执行 |
| 列表 | Space | 速览浮层（n/p 切换上下条） |
| 列表 | Tab | 开关右侧分栏预览（J/K 滚动预览） |
| 列表 | o / → | 全文阅读：j/k 滚动，d/u 半页，g/G 首尾，/ 查找，n/N 跳转，t 展开工具输出 |
| 列表 | y / Y | 复制会话 ID / 恢复命令 |
| 列表 | s / t / m | 切换来源 / 时间范围 / 机器筛选 |
| 列表 | / 或直接打字 | 回到搜索框 |
| 列表 | Ctrl-R | 刷新 |
| 任意 | 鼠标滚轮、点击 | 滚动、选择；再点一次已选中的会话即恢复 |

- 预览：搜索结果直接定位到命中的那条消息；普通会话显示开头和结尾；对话按 Markdown 排版，工具调用折叠显示。
- 多台机器同时查询，先到先显示，慢机器不拖累；状态行显示每台机器的条数或错误。
- 从快捷键打开时，Enter 会把恢复命令交给 shell 执行（会进入 shell 历史）；直接运行 `sessift` 时，Enter 直接执行。

启用快捷键（fish 放进 `fish_user_key_bindings` 函数里）：

```fish
command -q sessift; and sessift shell fish | source   # fish
eval "$(sessift shell zsh)"                            # zsh
eval "$(sessift shell bash)"                           # bash（只放到命令行，不自动执行）
```

换键：`sessift shell fish --key ctrl-o`。

## 命令

| 命令 | 作用 |
|---|---|
| `sessift search <词…>` | 搜索。`-m` 选机器（可重复，`all` 表示全部），`-n` 条数，`--source`、`--project`、`--cwd`、`--role`、`--session`、`--since`、`--until` 过滤 |
| `sessift sessions` | 按最后活跃时间列会话，`-n`、`--offset` 翻页 |
| `sessift read <会话 ID>` | 分页读正文。不带 `-m` 时自动在各机器上找这个会话；`--offset`、`-n`、`--max-chars` |
| `sessift`、`sessift pick` | 交互界面：搜索、预览、阅读、恢复会话 |
| `sessift shell fish\|zsh\|bash` | 输出快捷键绑定脚本 |
| `sessift machines` | 列出机器，测连通性、延迟和远端版本 |
| `sessift doctor` | 逐项诊断：后端、全新 SSH 握手耗时、往返延迟、样例查询耗时和传输量、broker 状态 |
| `sessift remote install <机器…>` | 安装或更新远端二进制 |
| `sessift broker status` / `stop` | 查看或停止后台 broker |
| `sessift index` | 建立或增量更新本机索引；`--rebuild` 重建，`index stats` 看各来源会话数、索引大小和待处理文件 |

时间参数支持 `7d`、`12h`、`2026-10-01`、`2026-10-01 09:30`、RFC 3339 和 Unix 时间戳。

通用选项：

- `--format text|jsonl|json`：终端里默认 `text`，管道里默认 `jsonl`。`json` 是一个完整文档，附带每台机器的耗时、传输字节数和错误。
- `--timing`：在 stderr 打印每台机器的耗时、远端处理时间、传输量、走的通道。
- `--no-broker`：不经过 broker，直接起 SSH（会用 OpenSSH 连接复用）。
- `--timeout 3s`：每台机器的截止时间，默认 5 秒。到时没回的机器给出警告，其余结果照常返回。

## 工作方式

```
sessift ──(进程内)──────────────> 本机后端
   │
   └─(Unix socket)─> broker ──(常驻 SSH 通道，帧协议 + zstd)──> 远端 sessift serve --stdio ──> 远端后端
```

- **broker**：第一次查询远端时自动在后台启动，对每台机器保持一条 SSH 通道，启动时预先连接默认机器，空闲 30 分钟退出。日志在 `~/.sessift/log/broker.log`。
- **断线和休眠**：请求途中连接断了，自动换新连接重发一次。连接空闲 20 秒以上时，下一个请求会同时发一个探测，1～2 秒内没回就判定连接已死（比如笔记本休眠唤醒后），立即重连重发。请求超时但连接可能只是慢时，在后台再确认一次，确实不通才换连接。
- **升级不打断查询**：换了新版本的 sessift 后，旧 broker 会在没有查询进行时自行退出，下次调用启动新版本。
- **只传结果**：远端查完再按字符预算截断，搜索只回前 K 条摘要，会话列表只回会话摘要，正文按页取。超过 1KB 的响应用 zstd 压缩。
- **可取消**：超时或放弃的请求会通知远端停止，并结束对应的后端进程。
- **容错**：远端登录 shell 打印的问候语会被跳过；远端没装 sessift、协议版本不一致、SSH 失败都会给出明确提示。

协议见 [docs/remote-protocol.md](docs/remote-protocol.md)。

## 配置

`~/.sessift/config.toml`（`SESSIFT_HOME` 可改目录）：

```toml
[defaults]
machines = ["local", "mm"]   # 不带 -m 时查哪些机器
timeout = "5s"

[backend]
kind = "native"              # 自研索引（默认）
# refresh_every = "2s"               # 查询前最多多久检查一次新记录；"0" 不自动更新
# max_concurrency = 4                # 远端同时执行的请求数

[broker]
# enabled = true
# idle_timeout = "30m"
# probe_after_idle = "20s"

[[machine]]
id = "mm"
label = "Mac mini"
ssh = "mm"                   # ~/.ssh/config 里的 Host 别名
# remote_command = "~/.sessift/bin/sessift"
# ssh_options = ["-o", "Compression=no"]
```

## 开发

```sh
cargo test                   # 单元测试 + 集成测试（用本地子进程模拟远端，不需要 SSH）
cargo clippy --all-targets
cargo fmt --check
scripts/bench.py --sessift target/release/sessift
```

自研索引的测试用 `tests/fixtures/home/` 里的虚构会话（布局和真实 HOME 一致，覆盖 Claude、Codex、Pi、OpenCode），见 `tests/native.rs` 和 `tests/cli.rs` 的 `native_backend_end_to_end`。

测试共 111 个：协议层（取消释放并发名额、版本不符、大响应压缩、远端崩溃）、多机合并与分页、broker（共享、退役替换、僵死连接、途中断线重试）、自研索引（各来源解析、增量更新、段合并）、交互界面（状态与按键、渲染、Markdown 排版与折行）、shell 绑定语法等。

集成测试用 `fixture` 后端（`SESSIFT_FIXTURE=<json>`）提供确定数据，`SESSIFT_FIXTURE_DELAY_MS` 注入延迟，`SESSIFT_TEST_NOISE=1` 模拟远端 shell 杂讯。
