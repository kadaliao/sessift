# sessift 开发计划

sessift 是一个命令行工具：搜索、浏览本机和其他机器上的 AI 编程助手会话记录（Claude Code、Codex、Pi、OpenCode 等）。
多台机器之间**不同步数据**，查询时到各台机器现场查，只把查好的结果传回来。

## 背景：为什么要重写

对 memex 0.31.1 的实测（2026-10-30，本机 MacBook + Mac mini `mm`）：

| 场景 | memex（本机 + mm） | memex（只查本机） |
|---|---|---|
| TUI 打开到出会话列表 | 20.3～21 秒 | 0.6 秒 |
| 搜索 | 1.2～2.1 秒 | 0.1 秒 |
| 会话列表 | 0.9 秒 | 0.2 秒 |

原因：

1. 每个远程请求新开一次 SSH，握手 1.1～1.8 秒；远端 memex 本身只要 0.08 秒。
2. TUI 首页让每台机器传回"最新 1 万条完整记录"再在本地拼会话：本机 293MB，mm 传了 86MB 还没传完，必然超时。
3. 查询线程串行且不可取消，启动时两次查询叠加成两次 10 秒超时。
4. 到 mm 的链路往返约 25ms，但带宽只有 30～180KB/s（流量经过代理），传输字节数决定快慢。

原型验证（一条常驻 SSH 通道，远端现场查、只传结果、zstd 压缩）：

| 请求 | 传输量 | 耗时 |
|---|---|---|
| 首次建通道（一次性） | — | 0.5 秒 |
| 搜索 20 条 | 3KB | 0.10～0.14 秒 |
| 首页 200 个会话 | 22KB | 0.38～0.45 秒 |
| 首页 50 个会话 | 4KB | 0.14～0.19 秒 |

## 设计原则

1. **远端只传结果。** 搜索传前 K 条和截断后的摘要，会话列表传会话摘要，正文按页、按字符预算取。任何响应都有字节上限。
2. **连接常驻。** 本机起一个后台中转进程（broker），对每台机器保持一条 SSH 通道，握手只付一次。命令行每次调用通过 Unix socket 找 broker，单次远程请求的开销约等于一次网络往返。
3. **压缩。** 帧体超过 1KB 就用 zstd 压缩。
4. **并发查询，按截止时间返回。** 本机和各远端同时查；超过截止时间的机器放弃，返回已有结果并给出警告，不会让一台慢机器拖住整体。
5. **请求可取消。** 客户端超时或放弃时通知远端，远端终止对应的查询（包括杀掉后端子进程）。
6. **协议和后端解耦。** 远程协议只描述 sessift 自己的数据模型。过渡期后端调用已安装的 memex，阶段 2 换成自研索引，协议和客户端不用改。
7. **可观测。** `--timing` 输出每台机器的耗时和字节数；`sessift doctor` 逐项诊断连接、版本、延迟。
8. **对异常情况健壮。** 远端 shell 往 stdout 打印杂讯、远端没装 sessift 或版本不对、SSH 断线、broker 进程残留，都要能识别并给出明确提示或自动恢复。

## 路线图

只做命令行工具，稳定后再做 agent 支持、herdr 支持，最后做 app。

| 阶段 | 内容 | 状态 |
|---|---|---|
| 1 | 远程查询层：协议、远端服务、broker、并发查询与合并、CLI 命令、远程安装、诊断；后端过渡期用 memex | 完成（2026-10-30） |
| 1.5 | 交互界面：参考 memex 的 TUI（Alt-M 打开），搜索、预览、全文阅读、一键恢复 | 完成（2026-10-30） |
| 2 | 自研索引：Claude / Codex / Pi / OpenCode 解析，增量入库，SQLite 会话表，Tantivy + 中英文混合分词（中文 bigram），正文按文件偏移按需读取，不复制原文 | **下一步**，方案见下文 |
| 3 | 搜索质量：排序调优、过滤项、多语言向量检索与 BM25 融合（可选） | 未开始 |
| 4 | 稳定化：文件监听增量索引、打包（Homebrew）、文档、长时间运行测试 | 未开始 |
| 5 | agent 支持（MCP / skill）、herdr 支持、app | 未开始 |

## 阶段 1：远程查询层

### 组件

```
sessift CLI ──(进程内)──> 本机后端（memex 适配器）
     │
     └──(Unix socket)──> broker ──(常驻 SSH 通道, 帧协议 + zstd)──> 远端 `sessift serve --stdio` ──> 远端后端
          （broker 不可用时，CLI 直接起 SSH，带 ControlMaster 复用）
```

- `sessift serve --stdio`：远端服务。从 stdin 读请求帧，并发执行，向 stdout 写响应帧，支持取消。
- `sessift broker`：本机后台中转。按需自动启动，空闲自动退出；版本不一致时自动重启。
- 并发查询与合并：搜索按分数合并，会话按最后活跃时间合并；截止时间到了就返回已有结果。
- 后端接口 `Backend`：`search` / `sessions` / `read` / `context` / `info`。实现：`memex`（过渡）、`fixture`（测试用，可注入延迟）。

协议细节见 [remote-protocol.md](remote-protocol.md)。

### 命令

| 命令 | 作用 |
|---|---|
| `sessift search <query>` | 搜索，支持 `--machine`、`--limit`、`--source`、`--project`、`--cwd`、`--role`、`--session`、`--since`、`--until` |
| `sessift sessions` | 按最后活跃时间列出会话，支持 `--limit`、`--offset` 和过滤项 |
| `sessift read <session-id>` | 分页读会话正文，`--offset`、`--limit`、`--max-chars` |
| `sessift machines` | 列出机器并测连通性、延迟、远端版本 |
| `sessift doctor` | 逐项诊断 |
| `sessift remote install <machine>` | 把本机二进制装到远端（同架构，压缩传输，哈希一致则跳过） |
| `sessift init` | 生成配置，可从 `~/.memex/config.toml` 导入机器 |
| `sessift broker status/stop` | 查看、停止 broker |

输出格式：终端里默认 `text`，管道里默认 `jsonl`，另有 `json`（带警告和各机器耗时的完整结构）。

### 性能目标（本机 ↔ mm 的链路）

| 场景 | 目标 |
|---|---|
| 远程搜索（broker 已连接） | P50 ≤ 200ms |
| 本机 + 远程合并的会话列表（50 条） | P50 ≤ 300ms |
| broker 首次连接 | ≈ 一次 SSH 握手 + 200ms |
| 一台机器无响应 | 截止时间（默认 5 秒）到了返回其余结果 |

过渡期远端每个请求仍要启动一次 memex 进程（约 80ms），阶段 2 换成自研索引后进一步下降。

### 步骤

1. 计划与协议文档。
2. 工程骨架：配置与路径（`SESSIFT_HOME`）、数据模型、输出格式。
3. 协议编解码：帧格式、前导魔数（跳过远端 shell 杂讯）、zstd、帧大小上限。单元测试。
4. 后端：`Backend` 接口、`fixture` 后端、`memex` 适配器（用真实 memex 输出做测试样本）。
5. 远端服务 `serve --stdio`：握手、并发、取消、并发上限、按预算裁剪。集成测试。
6. 客户端连接：SSH / 本地命令两种传输、握手、多路复用、超时、取消、错误提示（远端未安装、版本不符）。
7. 并发查询与合并、CLI 命令、三种输出格式、`--timing`。
8. broker：自动启动、单实例锁、预热、断线重连、空闲退出、版本不一致重启、失败时回退到直连。
9. `machines` / `doctor` / `remote install` / `init`。
10. 在 mm 上端到端验证，和 memex 对比性能。
11. 代码审查、修复、README。

### 测试策略

- 单元测试：编解码、合并、CJK 安全截断、memex 输出解析、配置。
- 集成测试：用"本地命令"传输代替 SSH，`fixture` 后端提供确定数据，覆盖握手、并发、取消、超时、杂讯、远端缺失、多机合并、broker 生命周期。
- 端到端：真实 SSH 到 mm，记录耗时并与 memex 对比。

### 阶段 1 结果（2026-10-30）

全部步骤完成（当时 61 个测试，加上交互界面后共 81 个）。和 memex 0.31.1 对比（本机 + mm，每项 10 次中位数）：

| 场景 | memex | sessift |
|---|---|---|
| 搜索，本机 + 远端 | 2533ms | 154ms |
| 搜索，只查远端 | 2669ms | 193ms |
| 50 个会话，本机 + 远端 | 1080ms | 145ms |
| 搜索，只查本机 | 102ms | 97ms |

计划外补充的可靠性机制：僵死连接探测、途中断线自动重试、broker 退役式替换、终端控制字符清理。

已知限制，留给后续阶段：

- 本机查询仍要启动 memex 进程（约 50～100ms），中文子串搜不到的问题也来自 memex 的分词。阶段 2 的自研索引解决。
- `remote install` 只支持两端同平台。
- 被字符预算截断的正文不能分段续读。

## 阶段 1.5：交互界面（2026-10-30 完成）

### 过程与决定

先做过一版 fzf 前端（`sessift pick` 驱动 fzf，fzf 通过 `--listen` 接收渐进加载的结果）。用户反馈"不是很方便"，要求参考 memex。对比后的差距：fzf 的通用列表可读性差；Enter 要按两次；没有筛选；预览是纯文本；没有会话内阅读和查找；两种模式切换不直观。于是删掉 fzf 版本，用 ratatui 写原生 TUI。

交互照 memex 的焦点模式：搜索框里打字即搜索（停顿 150ms 后发请求），↓ 或 Enter 进入列表，列表里单个字母是命令（Enter/r 恢复、e 放到命令行、Space 速览、Tab 分栏预览、o 全文阅读、s/t/m 筛选、y/Y 复制）。完整按键见 README。

### 结构（`src/tui/`）

| 模块 | 职责 |
|---|---|
| `app.rs` | 状态与按键处理，只产出 `Effect`（要发的请求），不做 I/O，便于单测 |
| `load.rs` | 每个请求一个后台任务，结果用 `Msg` 发回；中止任务即取消远端请求 |
| `view.rs` | 绘制主界面（搜索框、状态行、列表、分栏预览、速览浮层、底部提示）和全文阅读 |
| `md.rs` | 轻量 Markdown（标题、列表、引用、代码块、`code`、**粗体**）、查询词高亮、按显示宽度折行 |
| `theme.rs` | 256 色配色，保留终端背景色 |
| `mod.rs` | 终端进入与恢复（含 panic）、输入线程、事件循环 |

要点：

- 每台机器单独请求，先到先合并；新查询发出时中止旧请求；同一批结果陆续到达时保持选中项不跳。
- 列表请求用 `brief`；选中会话后再查完整信息拿恢复命令。
- 预览：搜索结果用 `context`（命中前 1 条、后 10 条），普通会话取开头 6 条加结尾 12 条；按会话和命中缓存。
- 全文阅读每页 300 条，滚到接近末尾自动加载下一页；从搜索结果打开时一直加载到命中那条并跳过去。
- 画在 /dev/tty 上，stdout 只用来把结果交给 shell 绑定：`run` 或 `edit` 一行，加命令一行。fish/zsh 绑定收到 `run` 时由 shell 执行，命令会进历史；bash 只能放到命令行。
- `sessift` 不带子命令且在终端里时等同 `sessift pick`。

### 验证方式

- 单测：`app.rs` 的状态与按键、`view.rs` 用 ratatui `TestBackend` 渲染、`md.rs` 的排版与折行。
- 端到端：在伪终端里起交互式 fish，按 Alt-M、输入、按 `e`/Enter 检查命令行（用 Python + pyte 截屏）。注意 pyte 不支持备用屏幕，且宽字符被半覆盖时不清另一半，截图里少空格、列错位、残影都是模拟器的问题，不代表真实终端。

### 和 memex 相比还没做的

- 首页顶部的活动图表（会话数或 token）。
- 按项目筛选的下拉菜单（目前靠搜索词）。
- 分享会话。

## 阶段 2：自研索引（下一步）

### 目标

- 去掉对 memex 的依赖。本机查询不再每次启动 memex 进程（约 50～100ms），在进程内直接查索引，目标 P50 < 30ms；会话列表 < 10ms。
- 中文词在句子中间也能搜到。验收用例：一条助手消息"在那之前回滚还来得及"，搜"回滚"必须命中（memex 的英文分词搜不到）。做成固定的测试样本。
- 不复制会话原文，索引体积 < 1GB（memex 是 5GB，其中 4.2GB 是原文副本）。
- 增量刷新：没有变化时 < 50ms；冷建全量（本机约 10GB 原始数据）< 2 分钟。

### 数据来源（本机实测规模）

| 来源 | 位置 | 规模 | 优先级 |
|---|---|---|---|
| Codex | `~/.codex/sessions`、`~/.codex/archived_sessions`（rollout JSONL） | 5.6GB + 0.3GB，约 360 个文件 | 1 |
| Claude Code | `~/.claude/projects/*/*.jsonl` | 2.3GB | 1 |
| Pi / Oh My Pi | `~/.pi`、`~/.omp/agent/sessions` | 1.5GB | 2 |
| OpenCode | `~/.local/share/opencode` | 92MB | 3 |

格式参考 memex 的解析器（github.com/nicosuave/memex 的 `src/sources/{claude,codex,pi,opencode}.rs`），但自己实现；先在本机抽样真实文件，整理出脱敏的测试样本。

### 存储设计（`~/.sessift/index/`）

- `sessions.sqlite`：
  - `sessions`：source、session_id、source_path、project、cwd、title、started_ms、last_ms、message_count、kind（主会话或子 agent）、parent_session。
  - `files`：path、inode、size、mtime、已解析到的字节偏移、解析器版本，用来做增量。
- Tantivy 索引，每条记录一个文档：
  - 存：record_id、session 引用、role、tool_name、ts（fast field），以及原文位置（文件 id + 字节偏移 + 长度）。
  - 只索引不存：正文。工具输出只索引前 8KB 左右，控制体积。
  - 摘要、预览、全文阅读都按位置从原文件读（pread 或 mmap）。原文件被删掉的会话，在列表里标出来。
- record_id 要稳定：用原生事件 id，没有的话用"文件路径 + 字节偏移"的哈希。

### 中英文混合分词

- 自定义 Tantivy tokenizer：字母和数字连续段转小写成词（英文可选轻量词干）；汉字、假名、韩文连续段切成重叠的两字词（bigram），只有一个字时保留单字。
- 查询用同一个分词器：中文词变成 bigram 的短语查询（带位置），这样"回滚"能命中"在那之前回滚还来得及"；单字查询用前缀匹配。
- 评分用 BM25；在 Codex 上验证工具输出噪音对排序的影响，必要时按 role 加权（用户和助手消息高于工具输出）。

### 解析与增量

- 每个来源一个解析器，输出：会话元数据，以及带字节偏移的记录（user、assistant、tool_use、tool_result）。
- JSONL 是追加写：只解析上次偏移之后的新内容；文件变小或 inode 变了就整文件重建。
- 标题：取第一条真正的用户消息，跳过 `/clear` 之类的命令、system reminder、`<command-name>` 等，截到 80 字。
- 项目名：从 cwd 往上找 git 仓库根目录，取目录名并缓存；找不到用 cwd 的最后一段。
- 恢复命令按来源生成，比如 Claude `cd '<cwd>' && claude --resume <id>`、Codex `codex resume <id>`。Pi、OpenCode 的写法要在本机核实。
- 刷新时机：查询前检查是否过期（目录 mtime 加文件 stat，代价很低）；`sessift index` 手动全量或增量。文件监听（FSEvents）留到阶段 4。
- 解析并行（rayon），写入单线程，批量提交。

### 接入

- `Backend::Native` 实现现有接口（search、sessions、read、context、info），配置 `backend.kind = "native"`。协议和 TUI 都不用改。
- 保留 memex 后端用于对比；新增 `sessift index [--rebuild]` 和 `sessift index stats`。
- 本机和 mm 都切到 native 之后，用 `scripts/bench.py` 和一组固定查询，对比 memex 的结果集和延迟。

### 步骤

1. 抽样本机四种来源的真实文件，整理格式要点和边界情况，做脱敏样本。
2. 记录模型、位置方案、分词器（先写测试，含"回滚"用例）。
3. Claude 和 Codex 解析器，含增量。
4. SQLite 表、Tantivy schema、入库流水线。
5. Native 后端的各个操作，摘要从原文读取。
6. Pi / omp、OpenCode。
7. `sessift index`，查询前自动刷新。
8. 两台机器切换到 native，做性能和召回对比，更新文档。
