# sessift

搜索、浏览、恢复 AI 编程助手会话（Claude Code、Codex、Pi、OpenCode、Grok 等）的命令行工具，覆盖本机和其他机器。机器之间不同步数据：查询时到各台机器现场查，只传结果。起因是 memex 在多机场景下太慢，见 `docs/plan.md` 的"背景"一节。

## 当前状态（2026-10-31）

- 已完成：远程查询层、交互界面（Alt-M），阶段 2 自研索引（`src/index/`）。本机和 mm 都已切到 native，和 memex 的延迟、结果集对比做完，memex 适配器已删除。结果见 `docs/plan.md` 的阶段 2。
- 2026-10-31 本机和 mm 都已卸载 memex（连同 herdr 插件），会话搜索只用 sessift。
- 阶段 3（搜索质量）完成了第 1～5 步和收尾，已合并到 main 并推送，两台机器已装上。结果见 `docs/plan.md` 的"阶段 3 结果"：评测 MRR@10 0.815 → 0.898，会话列表默认只列对话，新增 Grok 来源，边打字边搜。
- 2026-10-31 交互界面加了搜索框里的 Alt-s/t/m/a 筛选和鼠标支持，并发布第一个版本 v0.1.0（GitHub Release，附 macOS arm64 二进制）。
- 2026-10-31 可以用 Homebrew 安装：`brew install kadaliao/tap/sessift`，个人 tap 仓库 https://github.com/kadaliao/homebrew-tap （`Formula/sessift.rb`，直接装 Release 里的 arm64 二进制）。项目还没有许可证、也没有知名度，进不了 homebrew-core。
- 2026-10-31 新增 herdr 插件（`herdr/`，`prefix+m` 打开浮层，`sessift pick --herdr` 选中后在 herdr 里恢复），随 v0.2.0 发布（GitHub Release 和 Homebrew tap 已更新）。本机从 GitHub 装了插件，键绑在 dotfiles 里。改插件时先 `herdr plugin uninstall sessift`，再 `herdr plugin link <仓库>/herdr`，改完换回来。
- 2026-10-31 v0.3.0：agent skill（`sessift skill install`，本机已装）；agent 里运行时自动排除当前会话；`read --around`、`--tail`、`--tool-chars`；jsonl 时间改成带时区的本地时间。三个子 agent 试用过，见 `docs/plan.md` 的"阶段 5"。服务端也改了（`tool_chars`、`context` 的核对和 `next_offset`），mm 已同步更新。
- 2026-10-31 v0.3.1：修复 zsh 快捷键打开界面后收不到按键（crossterm `use-dev-tty`）；README 改用带按键的 GIF（`scripts/screenshots/run.sh` 生成）。mm 上 brew 装的 sessift 和远端服务都已更新，mm 也装了 skill、herdr 插件，配置里能查本机（本机 `~/.sessift/bin/sessift` 链接到 `~/.local/bin/sessift`）。
- 2026-10-31 v0.3.2：herdr 新开 tab 恢复时等提示符出来再敲命令（原来命令显示两遍）；浮层左边框压在中文上不再消失（sessift 的 `clear_overlay`，herdr 插件 0.1.1 浮层改成 100% 宽）。
- **待用户决定**：向量检索做不做（评测里换了说法的 7 条查询全部找不到，其余已全部进前 10）；要不要在索引时屏蔽明文密钥。
- 代码在 GitHub：https://github.com/kadaliao/sessift （公开仓库，文档和示例里不要出现私人环境信息和真实会话内容）。

## 文档

| 文件 | 内容 |
|---|---|
| `README.md` | 面向新用户的简要说明：功能、安装、按键、多机配置、命令、性能、截图（`docs/images/`）。不写开发细节，不提 memex |
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
| `src/herdr.rs` | `pick --herdr`：选中后经 herdr 命令行恢复（跳到已开的 pane、浮层下的空闲 shell、或新 tab） |
| `herdr/` | herdr 插件：manifest、`open.sh`（打开 popup）、`picker.sh`（找 sessift 并运行） |
| `src/skill.rs`、`skills/sessift/SKILL.md` | agent skill：SKILL.md 编进二进制，`sessift skill install` 写到 `~/.agents/skills` 并链接到 `~/.claude/skills` |
| `src/output.rs` | CLI 的 text、jsonl、json 输出 |
| `src/text.rs`、`src/timefmt.rs` | CJK 安全的截断与宽度、终端控制字符清理；时间解析与格式化 |
| `src/install.rs` | `sessift remote install` |
| `tests/` | `cli.rs` 端到端（本地命令代替 SSH），`native.rs` 自研索引，`protocol.rs` 协议层；`fixtures/home/` 是虚构的各来源会话 |
| `scripts/bench.py` | 命令行延迟基准；装了 memex 时同时测它作对照 |
| `scripts/eval.py` | 搜索质量评测，数据在 `~/.sessift/eval/`（真实内容，不进仓库） |
| `scripts/screenshots/` | README 的 GIF：`demo.py` 造虚构会话，`record.py` 在独立 tmux 里操作真实界面（还有一个假 HOME 里的 herdr），每按一次键截一帧，`gif.py` 用 Playwright 渲染、窗口下方画出按键，ffmpeg 合成 GIF，`term.py` 是共用的 ANSI 转 HTML，`run.sh` 串起来 |

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

- 集成测试用 `fixture` 后端（`SESSIFT_FIXTURE=<json>`）提供确定数据，`SESSIFT_FIXTURE_DELAY_MS` 注入延迟，`SESSIFT_TEST_NOISE=1` 模拟远端 shell 杂讯。自研索引的测试用 `tests/fixtures/home/` 里的虚构会话（布局和真实 HOME 一致）。
- 搜索质量评测 `scripts/eval.py`：一组"凭印象找某个会话"的查询（标注了目标会话）和一组主题查询，输出 MRR@10、前 10 命中数、前 10 里自动线程的个数和延迟。查询格式见脚本开头。排序参数可以用 `SESSIFT_RANK` 临时覆盖（如 `--env SESSIFT_RANK=cross=0,title=0`），在同一份索引上比较。
- README 的 GIF 用 `scripts/screenshots/run.sh` 重新生成：虚构的会话分在两台假机器上，在独立的 tmux 里跑真实的界面（herdr 那段用假 HOME 里单独的 herdr，只链接本仓库的插件），每按一次键截一帧，按键显示在窗口下方（需要 tmux、herdr、Chrome、ffmpeg、Playwright 和 Maple Mono NF CN 字体）。场景和每帧停留时间在 `record.py` 里改。改了界面后重新生成。

- 发版后更新 Homebrew：在 tap 的本地副本（`brew --repository kadaliao/tap`，或 `gh repo clone kadaliao/homebrew-tap`）里把 `Formula/sessift.rb` 的 `url` 改成新版本的包、`sha256` 换成 Release 附带的 `.sha256` 文件里的值，提交推送；然后 `brew audit --strict --online kadaliao/tap/sessift` 和 `brew test kadaliao/tap/sessift` 检查。新版 Homebrew 不加载没信任的 tap，`brew audit` 会直接拒绝，用全名 `brew install` 一次（会自动信任）或 `brew trust kadaliao/tap` 之后再跑。

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
- **shell 快捷键只对之后启动的 shell 生效**：herdr 的 pane 会活好几天，改了 `fish_user_key_bindings` 后旧 pane 要 `exec fish`。agent pane 里的 Alt-M 被 agent 收走，所以 herdr 里另用插件。
- **herdr 插件的 popup 没有 pane ID**：拿不到 `HERDR_PANE_ID`，浮层下面的 pane 从 `HERDR_PLUGIN_CONTEXT_JSON` 的 `focused_pane_id` 取。popup 不能用 `pane send-keys` 操作，测试时直接在普通 pane 里跑 `sessift pick --herdr` 并手动设这个环境变量；popup 用 socket 的 `popup.close` 关。herdr 命令出错时把 JSON 错误写在 stdout、退出码 1。
- **agent 里跑 sessift 会自动排除当前会话**：`CLAUDE_CODE_SESSION_ID`、`CODEX_THREAD_ID`、`PI_SESSION_ID`、`GROK_SESSION_ID` 里的会话（连同子 agent）不出现在 `search`/`sessions` 里。在 Claude 里调试搜索结果时记得这一点，需要时加 `--include-current`。`tests/cli.rs` 会清掉这些变量。
- **record_id 只在本机有效**：里面编的是本机索引的内部会话号，`context` 请求要带 `session_id`，服务端会核对。
- **zsh 的快捷键 widget 里标准输入是 `/dev/null`**，crossterm 只能打开 `/dev/tty`，而 macOS 的 kqueue 监听不了它，界面能画出来但收不到按键。所以 crossterm 开了 `use-dev-tty` 特性（改用 poll）。fish 的 `(…)` 继承真正的终端，不受影响。
- **tmux 的 `new-session -e PATH=…` 不一定生效**：录 GIF 的 shell 在自己的 rc 里设 PATH，tmux 服务器用干净的环境启动（也避免继承 `HERDR_*` 连到真实的 herdr）。
- **herdr 新开 tab 后不能马上 `pane run`**：shell 还没进入行编辑时，敲进去的字会先被终端原样回显一遍，提示符出来后 shell 又显示一遍。`herdr.rs` 先读 pane 等屏幕出现内容、稳定 150ms（最多 3 秒）再敲。
- **浮层的左边框压在双宽字上会消失**：ratatui 输出差异时，一个双宽字后面那一格直接跳过，浮层左边一格的中文跨进来，就把边框那一格吃掉了。sessift 的浮层用 `view.rs` 的 `clear_overlay`（把跨进来的字换成空格）；herdr 0.9.5 的 popup 有同样的问题，插件把浮层宽度设成 100%，让左边框挨着侧栏的单宽分隔线。
- **超时不等于连接死了**：慢链路上大响应本来就可能超时。broker 只在后台探测确认不通之后才换连接，否则重连会越来越慢。
