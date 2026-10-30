# 会话记录格式

各来源的格式要点和解析规则，是 `src/index/source/` 的依据。2026-10 在作者的机器上用真实文件核对：Claude Code 2.1.31x、codex-cli 0.164、Pi 1.4、OpenCode 1.19。测试样本在 `tests/fixtures/`，内容全部虚构。

## 共同约定

- 记录有四种角色：`user`、`assistant`、`tool_use`、`tool_result`。思考过程（thinking、reasoning）、系统提示、工具定义和注入的上下文都不当作记录。
- **记录的位置**：JSONL 来源用"行首字节偏移 + 行长 + 行内序号"，压缩文件按解压后的偏移算。OpenCode 用"part 在会话中的序号 + 子序号"（工具 part 的调用是 0，结果是 1）。
- **记录 ID**：`<会话键>-<偏移>-<序号>`，只要原文件不被改写就保持稳定。
- **解析约定**：一行产生哪些记录、文字是什么，只能取决于这一行本身。跨行状态只能用来补元数据（工具名、标题、cwd），或者整行跳过。读回记录时，单独重新解析那一行，按序号取对应的记录。测试会对每个样本核对这一点。
- **未写完的末行**：普通文件里没有换行结尾的最后一行可能还在写，留到下次再解析。压缩文件已经写完，末行照常解析。中间有坏行就跳过并计数。
- **图片**：图片和 base64 数据永远不进索引。Claude 的工具输出里图片占原始字节的 35%，Codex 占 53%，Pi 占 78%。
- **标题**：取第一条真正的用户消息，压成一行，最多 80 字。各来源的例外见下文。
- **消息数**：用户消息加助手消息的条数。

## Claude Code

- **位置**：`$CLAUDE_CONFIG_DIR/projects` 或 `~/.claude/projects`。
  - 主会话：`<编码后的 cwd>/<会话 id>.jsonl`。
  - 子 agent：`<编码后的 cwd>/<会话 id>/subagents/agent-<id>.jsonl`，旁边的 `.meta.json` 里有 `description`（作标题）和 `isFork`。
  - 其他文件一律忽略。
  - 目录名的编码会丢信息（中文变成 `-`），cwd 要从行里取。
- **会话 ID**：主会话是文件名；子 agent 是 `agent-<id>`，父会话是所在目录名。
- **会话种类**：
  - `subagent`：子 agent 文件。
  - `background`：第一条带 `parentUuid` 的行，其 `sessionKind` 是 `bg`、`daemon` 或 `daemon-worker`（`claude --bg`）。
  - `main`：其余情况。
- **只解析本会话自己的行**：
  - 桌面版 fork 出来的会话，开头是原会话的逐字拷贝，这些行的 `sessionId` 是原会话的。
  - fork 时连带拷贝的子 agent 文件，整个文件的 `sessionId` 都不是本会话的。
  - fork 子 agent（`isFork`，或第一行是 `fork-context-ref`）的继承部分，到第一条包含 `<fork-boilerplate>` 的用户行为止。
  - 规则：`sessionId` 不是本会话的行不产生记录，fork 子 agent 在继承部分结束前也不产生记录，但仍然从这些行里收集工具名。
- **按 `type` 分派**：
  - `assistant`：每个 `text` 块是一条助手记录，每个 `tool_use` 块是一条工具调用。`thinking` 跳过。`model` 为 `<synthetic>` 或带 `isApiErrorMessage` 的行是客户端报错，跳过。
  - `user`：带 `isMeta` 或 `isCompactSummary` 的整行跳过。`tool_result` 块是工具结果，工具名通过 `tool_use_id` 找。`text` 块是用户消息，但 `origin.kind` 不是 `human` 时跳过（任务通知、其他会话发来的消息）。
  - `attachment`：只有 `queued_command`（用户在 agent 忙时打的字）而且 `origin` 是人时，才是用户消息。
  - `custom-title`、`ai-title`、`agent-name`、`summary`：标题候选，后出现的覆盖先出现的。
  - 其余类型跳过，包括 `api-request-blob`。它占字节数的 17%，`message` 里是请求内容的副本。
- **用户文字的处理**：
  - 以 `<system-reminder>`、`<task-notification>`、`<local-command-caveat>`、`[Request interrupted by user` 开头的跳过。
  - `<command-name>/x</command-name>…<command-args>a</command-args>` 变成 `/x a`。
  - `<bash-input>c</bash-input>` 变成 `! c`。
  - `<local-command-stdout>`、`<bash-stdout>` 取内容，内容为空则跳过。
  - `<pasted_content id="…">` 拆掉外壳；注意闭合标签也带 id。
  - `<fork-boilerplate>…</fork-boilerplate> Your directive: X` 变成 `X`。
- **工具调用的文字**：
  - Bash：`# 说明` 加命令。
  - Read：路径。
  - Edit：路径加 `- 旧` / `+ 新`。
  - Write：路径加内容。
  - Agent：说明、类型和提示。
  - 其他工具（含 MCP）：每个字段一行 `key: value`。
- **工具结果的文字**：取 `message.content`，可能是字符串，也可能是文本块列表，图片显示为 `[image]`。不要用 `toolUseResult`，那是界面用的更大的副本。
- **标题优先级**：
  1. 自定义标题。
  2. 子 agent 的 description。
  3. 第一条真正的提问（Claude Code 自己的规则：跳过以标签开头的消息、被打断的消息，记下第一个斜杠命令作为兜底，去掉 `[Image #N]`）。
  4. AI 生成的标题、agent 名、旧版 summary。
  5. 最先出现的斜杠命令。
- **恢复**：`cd '<cwd>' && claude --resume <id>`；子 agent 恢复它的父会话。
- **增量**：文件只追加，但 Claude Code 也有整文件改写的路径，所以每次核对上次解析位置前 256 字节的指纹，对不上就整文件重解析。

## Codex

- **位置**：`$CODEX_HOME` 或 `~/.codex` 下的 `sessions/YYYY/MM/DD/rollout-<时间>-<uuid>[_<代>].jsonl` 和 `archived_sessions/`。
  - 会话闲置约 7 天后，Codex 会把它压缩成同名的 `.jsonl.zst`：单个 zstd 帧，帧头带解压后大小，修改时间保留，原 `.jsonl` 删除。压缩前后内容逐字节相同，所以偏移不变。
  - 入库时把这种情况当作改名：解压后大小和原文件一致，就只改路径，记录 ID 保持不变。
  - 旧的 `.json`（2025 年 4 月之前的 TS 版）不支持。
- **会话 ID**：取第一行 `session_meta.payload.id`。不要用 `payload.session_id`，在子线程里它是根线程的 id。
  - 一个会话可能分成多个文件（`_<代>` 后缀，本机 11 个会话），目前各文件分别列出。
- **会话种类**（看 `payload.source` 和 `thread_source`）：
  - `guardian`：自动审批的复核线程。
  - `subagent`：父会话见 `parent_thread_id` 或 `source.subagent.thread_spawn.parent_thread_id`。
  - `fork`：有 `forked_from_id`。
  - `exec`：`source` 为 `exec`，即 `codex exec`。
  - `main`：其余情况。
- **复制来的父会话历史**：subagent、guardian、fork 文件开头是父会话历史的拷贝。跳过它们，直到第一条 `turn_context` 或 `event_msg/task_started` 的 `turn_id` 是 UUIDv7 且不小于本会话 id（UUIDv7 的字典序就是时间序）。文件里的 `subagent_history_start_ordinal` 在迁移过的文件里不可靠。
- **记录只取 `response_item` 行**：`event_msg` 是同一内容的重复渲染，全部跳过（解析前按行首字节就能跳过，省掉三分之一的解析量）。
  - `message`，角色是用户：用户消息。注入的上下文按 part 去掉：
    - 有 `content_item_kinds` 而且长度和 part 数一致时，只留 `user.*` 类型的 part。
    - 没有时按前缀去掉 `<environment_context>`、`# AGENTS.md instructions`、`<turn_aborted>`、`<skill>` 等，完整列表见 `codex.rs`。
    - 只含图片包装的 part 也去掉。
  - `message`，角色是助手：文字 part 用空行连接。`developer`、`system` 角色跳过。
  - `function_call`、`custom_tool_call`、`local_shell_call`、`web_search_call`、`tool_search_call`、`image_generation_call`：工具调用。
    - 工具名：如果 `namespace` 以 `mcp__` 开头，工具名是 `namespace__name`。
    - 文字：`exec_command` 取 `cmd`；`shell` 的 `[bash, -lc, X]` 取 `X`；`apply_patch` 取补丁原文；其他取参数 JSON。
  - `*_output`：工具结果。`{"output": …, "metadata": …}` 形式的字符串取 `output`；数组里的图片显示为 `[image]`。
  - `agent_message`：多 agent 之间的消息，只取明文部分，当作工具结果。
  - `reasoning`、`additional_tools` 等跳过。
- **标题**：第一条用户消息。IDE 或桌面版的包装（`## My request for Codex:`、`## My request:`）只取最后一段。以 `<heartbeat>`、`<codex_delegation>` 开头的不算。subagent 没有用户消息时，用 `agent_path`。
- **恢复**：`cd '<cwd>' && codex resume <id>`；subagent 和 guardian 恢复父会话。
- **增量**：`.jsonl` 只追加。`.zst` 不再变化，一旦变化就整文件重解析。

## Pi

- **位置**：
  - 根目录：`$PI_CODING_AGENT_SESSION_DIR`；否则 `$PI_CODING_AGENT_DIR/sessions`；都没有时是 `~/.pi/agent/sessions`。
  - 会话文件：根目录下的 `<编码后的 cwd>/<时间>_<id>.jsonl`，只有两层，一个文件一个会话。
  - `artifacts/` 下的文件不是会话。`~/.pi` 里的上万个文件大多是 npm 包，与会话无关。
- **第一行**：`{"type":"session","id","timestamp","cwd"[,"parentSession"]}`，cwd 以这里为准。
- **会话种类**：
  - `fork`：头部有 `parentSession`（父会话文件路径）。复制来的条目时间早于头部时间，跳过它们。
  - `subagent`：文件名里的 id 和头部 id 不同，文件名形如 `<子 agent uuid>-<8>-<8>-<4>`。父会话从 `artifacts/<父 id>/subagent-scripts/*-<子 agent uuid>.sh` 找。
  - `main`：其余情况。
- **记录**：
  - `message`，角色是用户：文字块是用户消息。
  - `message`，角色是助手：所有文字块合成一条助手记录，每个 `toolCall` 是一条工具调用（参数用 JSON）。
  - `message`，角色是 `toolResult`：工具结果，工具名取 `toolName`，出错时加前缀 `Error: `。
  - `message`，角色是 `bashExecution`：工具结果。
  - `custom_message`（子 agent 回报等）：工具结果，工具名记作 `custom:<类型>`。
  - `compaction`：助手记录。
  - `system` 角色、thinking 块、图片跳过。
- **标题**：
  - 用户用 `/name` 起的名字（`session_info`）优先。
  - 否则取第一条用户消息：`<skill name="X">…</skill>` 换成 `/skill:X `，开头的 `<file>` 块去掉（全是文件块时，取第一个文件块的内容）。
- **恢复**：`pi --session '<文件路径>'`，在任何目录下都能用。
- **增量**：文件只追加。崩溃修复后，文件中间可能留下一行坏行。

## OpenCode

- **位置**：`$OPENCODE_DATA_DIR`（可以用逗号分隔多个）或 `~/.local/share/opencode`，读其中的 `opencode*.db`。
  - 用 SQLite 只读模式打开。不用 `immutable`，那样会看不到 WAL 里已提交的数据；也不做 checkpoint。
  - `storage/` 是旧版 JSON 存储，新版已迁入数据库，不读。
- **会话**：`session` 表一行就是一个会话。`parent_id` 非空的是 `subagent`，cwd 取 `directory`。
- **记录**：
  - 读取顺序：消息按 `time_created, id`，part 按 `id`。
  - `text` part：用户消息（`synthetic` 或 `ignored` 的跳过）或助手消息。
  - `tool` part：产生两条记录。第一条是调用：有 `command` 时取命令，否则取输入的 JSON。第二条是结果，状态 `completed` 时取输出，`error` 时取 `Error: <错误>`，还在运行的没有结果。
  - `reasoning`、`step-*`、`patch`、`file` 跳过。
- **标题**：用 `session.title`。如果是 OpenCode 的占位标题（`New session - <ISO 时间>`），改用第一条非 synthetic 的用户消息。
- **恢复**：`cd '<directory>' && opencode --session <id>`。
- **增量**：
  - 先比较数据库文件和 `-wal` 文件的大小与修改时间；不能看 `-shm`，读操作也会改它。
  - 有变化时，对每个会话算一个签名：会话行，加上 `message` 和 `part` 各自的条数与最大 `time_updated`（约 2ms）。
  - 签名变了的会话整个重读；数据库里没有了的会话删除。
  - `session.time_updated` 不会随新消息更新，不能单靠它判断。

## Oh My Pi（omp）

本机和 mm 上都没有 omp 的数据，暂不支持。memex 索引里曾经有的 omp 会话，原文件已经不在了。
