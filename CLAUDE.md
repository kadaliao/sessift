# sessift

搜索、浏览、恢复 AI 编程助手会话（Claude Code、Codex、Pi、OpenCode、Grok 等）的命令行工具，覆盖本机和其他机器。机器之间不同步数据：查询时到各台机器现场查，只传结果。起因是 memex 在多机场景下太慢，见 `docs/plan.md` 的"背景"一节。

## 当前状态（2026-10-31）

- 已完成：远程查询层、交互界面（Alt-M），阶段 2 自研索引（`src/index/`）。本机和 mm 都已切到 native，和 memex 的延迟、结果集对比做完，memex 适配器已删除。结果见 `docs/plan.md` 的阶段 2。
- 2026-10-31 本机和 mm 都已卸载 memex（连同 herdr 插件），会话搜索只用 sessift。
- 阶段 3（搜索质量）完成了第 1～5 步和收尾，已合并到 main 并推送，两台机器已装上。结果见 `docs/plan.md` 的"阶段 3 结果"：评测 MRR@10 0.815 → 0.898，会话列表默认只列对话，新增 Grok 来源，边打字边搜。
- 2026-10-31 交互界面加了搜索框里的 Alt-s/t/m/a 筛选和鼠标支持，并发布第一个版本 v0.1.0（GitHub Release，附 macOS arm64 二进制）。
- **待用户决定**：向量检索做不做（评测里换了说法的 7 条查询全部找不到，其余已全部进前 10）；要不要在索引时屏蔽明文密钥。
- 代码在 GitHub：https://github.com/kadaliao/sessift （公开仓库，文档和示例里不要出现私人环境信息和真实会话内容）。

## 文档

| 文件 | 内容 |
|---|---|
| `README.md` | 用法、按键、命令、配置、性能数据 |
| `docs/plan.md` | 背景、设计原则、路线图、各阶段方案与结果，阶段 2 的详细方案 |
| `docs/remote-protocol.md` | 远程协议：帧格式、握手、操作、错误码、broker 消息、连接可靠性 |
| `docs/formats.md` | Claude、Codex、Pi、OpenCode、Grok 会话记录的格式要点和解析规则 |

## 代码地图

| 文件 | 职责 |
|---|---|
| `src/main.rs` | 入口，起 tokio 运行时 |
| `src/cli.rs` | clap 命令与各命令的处理；不带子命令时进入 TUI |
| `src/config.rs` | 路径（`SESSIFT_HOME`）、`config.toml`、机器描述 `MachineSpec`、从 memex 配置导入、build_id |
| `src/model.rs` | 与后端无关的数据模型和取值上限（请求、会话摘要、记录、页） |
| `src/proto.rs` | 线上消息、操作、错误码；对未知消息向前兼容 |
| `src/codec.rs` | 帧编解码、zstd、跳过远端 shell 杂讯的前导魔数 |
| `src/rpc.rs` | `RpcCore`（客户端多路复用、超时、丢弃即取消）、`Inflight`（服务端每个请求恰好一个回复） |
| `src/server.rs` | `sessift serve --stdio`，远端服务 |
| `src/client.rs` | 到远端的连接（SSH 或本地命令）、握手、启动失败分类、`ssh_base`（ControlMaster 复用） |
| `src/broker.rs` | 本机后台中转：自动启动、单实例锁、预热、重连、僵死探测、空闲退出、退役式替换 |
| `src/fanout.rs` | `Runner`：本机进程内、经 broker 或直连；结果合并 |
| `src/backend/` | `Backend` 接口；`native.rs`（自研索引），`fixture.rs` 测试后端 |
| `src/index/` | 自研索引：`source/` 各来源解析器，`ingest` 增量入库，`query` 查询，`rank` 会话排序规则与 `SESSIFT_RANK`，`store` SQLite，`fulltext` Tantivy、收集器与段合并策略，`directory` Tantivy 文件目录，`tokenizer` 中英文分词，`reader` 按位置读原文 |
| `src/tui/` | 交互界面：`app` 状态与按键，`load` 后台请求，`view` 绘制，`md` 排版，`theme` 配色 |
| `src/pick.rs` | 恢复命令、按 shell 转义、剪贴板、shell 快捷键脚本 |
| `src/output.rs` | CLI 的 text、jsonl、json 输出 |
| `src/text.rs`、`src/timefmt.rs` | CJK 安全的截断与宽度、终端控制字符清理；时间解析与格式化 |
| `src/install.rs` | `sessift remote install` |
| `tests/` | `cli.rs` 端到端（本地命令代替 SSH），`native.rs` 自研索引，`protocol.rs` 协议层；`fixtures/home/` 是虚构的各来源会话 |
| `scripts/bench.py` | 命令行延迟基准；装了 memex 时同时测它作对照 |
| `scripts/eval.py` | 搜索质量评测，数据在 `~/.sessift/eval/`（真实内容，不进仓库） |

## 常用命令

```sh
cargo fmt --check && cargo clippy --all-targets && cargo test   # 提交前必须全部通过
cargo build --release
install -m 755 target/release/sessift ~/.local/bin/sessift      # 本机安装
sessift remote install mm                                       # 更新 mm 上的远端服务（改了协议或后端必须做）
sessift doctor                                                  # 连接、版本、延迟诊断
scripts/bench.py --sessift target/release/sessift               # 延迟基准
scripts/eval.py --sessift target/release/sessift --compare defaults   # 搜索质量，改排序或解析后跑
```

测试里所有数据都在临时目录（`SESSIFT_HOME`），不会碰真实的 `~/.sessift`；测试结束会停掉自己起的 broker。

## 约定

- 文档、界面文案和提交说明都用中文。
- 协议要向前兼容：新字段加 `#[serde(default)]`；新增的通知类消息，旧端收到会忽略；新增操作，旧端回 `unsupported`，客户端要能降级（参考 `context` 的处理）。
- 所有远程操作都是只读的，所以 broker 可以换连接重试。新增有副作用的操作时要重新考虑这一点。

## 环境

本机和远端机器的具体信息（机器、链路、快捷键配置位置、用户的提交身份规则）在 `CLAUDE.local.md`，这个文件不进 git。没有这个文件时，按 `README.md` 用 `sessift init` 生成配置。

## 踩过的坑

- **新二进制首次执行慢**：macOS 对刚编译或刚复制的二进制，第一次 exec 要多花约 0.6 秒做安全检查。测试计时前先执行一次预热；不要在异步任务里同步等子进程 exec（`spawn_broker` 用的是 `spawn_blocking`）。
- **Unix socket 路径不能超过 104 字节**。会话临时目录的路径太长，测试里的 socket 要放在 tempfile 目录。
- **用伪终端截屏验证 TUI 时**：pyte 不支持备用屏幕，而且宽字符被覆盖一半时不会清掉另一半，截图里少空格、列错位、残影都是模拟器的问题。界面是否正确以 ratatui `TestBackend` 的单测为准。
- **fish 和 POSIX 的单引号规则不同**：fish 单引号里 `\\` 和 `\'` 是转义，POSIX 用 `'\''`。`pick::quote` 按 shell 区分。远端恢复命令会经过本地 shell、ssh、远端 fish 三层。
- **通过 ssh 在远端跑脚本**：一律包成 `sh -c '…'`，脚本里不能有单引号（见 `install.rs`），因为远端登录 shell 是 fish。
- **远端 shell 杂讯**：登录脚本可能往 stdout 打印东西。服务端先写魔数，客户端跳过魔数之前的所有内容；`remote install` 只认 `SESSIFT_*=` 开头的行。
- **macOS 上 Rust 的 `sync_data`/`sync_all` 是 `F_FULLFSYNC`**，每次约 5ms（普通 `fsync` 0.03ms）。Tantivy 一次提交同步十几个文件，所以 `directory.rs` 换成普通 `fsync`。别把 `Index::open_in_dir`/`create_in_dir` 用回来。
- **Tantivy 的合并会拖住提交它的那次刷新**：`wait_merging_threads` 要等合并做完，默认策略偶尔合并四分之一的索引。查询里的刷新用 `Merges::Small`，大合并交给 `sessift index`。
- **zst 文件的"已解析字节"是解压后的大小**，比文件本身大，不能拿来和文件大小比较判断"变短了"。
- **OpenCode 的变化要看 `-wal` 文件**：提交先写进 WAL，数据库文件要到 checkpoint 才变；`-shm` 读一下也会变，不能用。
- **Grok 的标题和最后活跃时间在 `summary.json`**，常比对话记录晚几秒到一个多小时才写，所以入库判断变化时把它的修改时间也算上（`ingest.rs` 的 `scan`）。
- **Claude 和 Codex 的 fork、子 agent 文件开头是复制来的父会话历史**，不跳过会重复命中，规则见 `docs/formats.md`。
- **开发时别直接索引全部真实数据**：用 `tests/fixtures/home` 或临时 HOME 里的部分文件（`cp -c` 克隆）加临时 `SESSIFT_HOME`，全量只在最后验证性能时跑。
- **改了解析器版本（`VERSION`）后，新旧两个二进制会互相把对方的结果重新解析一遍**（版本不同就整个来源重解析）。开发时用临时 `SESSIFT_HOME` 跑新版本，确定后再装到 `~/.local/bin`。
- **评测会被自己污染**：在 Claude 里读会话、写查询的那个会话和它的子 agent 会话，正文里就有目标会话的内容和查询词，会排到最前。把这些会话的 ID 加进 `~/.sessift/eval/exclude.txt`。
- **发版的二进制会带上本机路径**：Rust 标准库和依赖的源码路径会编进二进制（几百处家目录路径）。发版时用 `RUSTFLAGS="--remap-path-prefix=$HOME=~ --remap-path-prefix=$PWD=sessift"`，加单独的 `CARGO_TARGET_DIR` 编译（免得冲掉平时的编译缓存），打包前用 `strings` 确认没有家目录路径。
- **超时不等于连接死了**：慢链路上大响应本来就可能超时。broker 只在后台探测确认不通之后才换连接，否则重连会越来越慢。
