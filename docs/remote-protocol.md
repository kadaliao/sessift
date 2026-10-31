# 远程协议（protocol 1）

sessift 的客户端（CLI 或 broker）和远端服务 `sessift serve --stdio` 之间的协议。传输层是任意双向字节流，通常是 `ssh -T host '<sessift> serve --stdio'` 的 stdin/stdout。broker 和 CLI 之间的 Unix socket 用同一套帧格式（消息集合不同，见文末）。

## 字节流

1. 服务端启动后先向 stdout 写 9 字节魔数 `00 53 45 53 53 49 46 54 00`（`\0SESSIFT\0`），之后只写帧。
2. 客户端读取时丢弃魔数之前的所有字节（远端登录 shell 可能打印问候语等杂讯），最多丢弃 64KB；超过则报错并附上收到的杂讯，方便排查。
3. 客户端发往服务端的方向没有魔数，直接是帧。

## 帧

```
+----------------+---------+----------------------+
| 长度 N (u32 BE) | flags u8 | body (N-1 字节)       |
+----------------+---------+----------------------+
```

- `N` 包含 flags 字节，不含长度字段本身。上限 32 MiB，超出即视为协议错误并断开。
- `flags` 第 0 位为 1 表示 body 经过 zstd 压缩。其余位保留，必须为 0。
- body 是 UTF-8 JSON 对象。
- 只有在对方的 hello 里声明了 `"zstd"` 特性之后才能发送压缩帧；hello 帧本身一律不压缩。发送方对超过 1KB 的 body 压缩，压缩后没有变小就发原文。

## 握手

客户端连接后第一帧必须是 hello：

```json
{"t":"hello","protocol":1,"version":"0.1.0","features":["zstd"]}
```

服务端回复：

```json
{"t":"hello","protocol":1,"version":"0.1.0","host":"mm.local","features":["zstd"],
 "backend":{"kind":"native","version":"0.1.0 1234 sessions"}}
```

协议号不一致时服务端回 `fatal`（`code: "protocol_mismatch"`，附带自己的协议号和版本）然后退出。客户端据此提示用户运行 `sessift remote install <machine>`。

## 请求与响应

请求：

```json
{"t":"req","id":7,"op":{"kind":"search","query":"部署","limit":20,"snippet_chars":240,"filters":{"source":"claude"}}}
```

- `id` 由客户端分配，同一连接内唯一。响应可以乱序返回，客户端按 `id` 匹配。
- 服务端并发处理请求，有并发上限（默认 4），超出的排队。

成功响应：

```json
{"t":"ok","id":7,"took_ms":83,"reply":{"kind":"search","hits":[...]}}
```

失败响应：

```json
{"t":"err","id":7,"code":"backend","message":"record 9f3c0a1b2c3d4e5f-1a2b-0 not found"}
```

取消：

```json
{"t":"cancel","id":7}
```

服务端收到后终止该请求（包括杀掉后端子进程），并回复 `code: "cancelled"` 的 `err`。对已完成或未知的 `id`，取消是空操作。

连接级错误：

```json
{"t":"fatal","code":"protocol","message":"first frame must be hello"}
```

发出 `fatal` 后服务端关闭连接。客户端关闭 stdin 时，服务端取消所有进行中的请求并退出。

## 错误码

| code | 含义 |
|---|---|
| `bad_request` | 参数不合法或超出上限 |
| `unsupported` | 不认识的 op（对方版本更新），客户端可降级 |
| `backend` | 后端执行失败 |
| `cancelled` | 被客户端取消 |
| `internal` | 服务端内部错误 |
| `protocol` / `protocol_mismatch` | 连接级错误，随 `fatal` 发送 |

## 操作

时间统一为 Unix 毫秒（整数）。字符数按 Unicode 标量计，截断不会切断字符。

### `ping`

```json
{"kind":"ping"}  →  {"kind":"pong","server_time_ms":1793345000000}
```

### `info`

```json
{"kind":"info"}  →  {"kind":"info","host":"mm.local","version":"0.1.0","protocol":1,
                     "backend":{"kind":"native","version":"0.1.0 1234 sessions"},
                     "uptime_ms":12345,"active_requests":0}
```

### `search`

参数：`query`（必填）、`limit`（1～200，默认 20）、`snippet_chars`（16～2000，默认 240）、`filters`、`unique_sessions`（默认 `false`，为 `true` 时每个会话只返回分数最高的一条，交互界面用它）。

`filters` 各字段都可省略：`source`、`project`、`cwd`、`role`、`session_id`、`since_ms`、`until_ms`、`kind`、`exclude_sessions`。`exclude_sessions` 是会话 ID 列表，这些会话连同从它们派生的子 agent、fork 会话（按 `parent` 递归）都不返回；命令行在 agent 里运行时用它排除 agent 自己的会话。旧版服务端会忽略这个字段。

旧版服务端不认识 `unique_sessions` 会直接忽略，所以客户端拿到结果后仍按会话去重一次。

```json
{"kind":"search","hits":[{
  "score":28.03,"ts_ms":1785397215000,"source":"claude","session_id":"7c1e…",
  "source_path":"/Users/…/7c1e….jsonl","record_id":"rid1_…","project":"shop-api",
  "role":"assistant","snippet":"PR 已合并，开始部署 shop-api。"}]}
```

### `sessions`

参数：`limit`（1～500，默认 20）、`offset`（默认 0）、`filters`（同上，`role` 不适用）、`brief`（默认 `false`）。按 `last_ms` 倒序。

`brief` 为 `true` 时省略列表用不到的 `cwd`、`resume_cmd`、`kind`、`started_ms`，这几个字段占了每行的大头，在慢链路上能明显减少传输量。交互界面的列表用 `brief`，选中会话后再单独查一次完整信息拿恢复命令。

```json
{"kind":"sessions","sessions":[{
  "source":"claude","session_id":"a1b2…","source_path":"/Users/…/a1b2….jsonl",
  "project":"sessift","cwd":"/Users/…/sessift","title":"给会话搜索做交互界面",
  "started_ms":1793344716000,"last_ms":1793346667000,"message_count":190,
  "kind":"main","resume_cmd":"cd … && claude --resume a1b2…"}]}
```

### `read`

参数：`session_id`（必填）、`source_path`、`offset`（默认 0）、`limit`（1～500，默认 50）、`max_chars`（整页正文字符预算，256～200000，默认 16000）、`tool_chars`（可选：分配预算之前，每条工具调用和工具输出先截到这么多字符，免得长输出挤掉对话；不填就不截。命令行默认 1500，交互界面不填。旧版服务端会忽略）。

```json
{"kind":"page","session_id":"a1b2…","source_path":"/Users/…","offset":0,"total":192,"next_offset":3,
 "records":[{"record_id":"rid1_…","ts_ms":1793344716325,"role":"user","tool_name":null,
             "text":"帮我看看这个项目为什么慢…","total_chars":98,"truncated":false}]}
```

`next_offset` 为 `null` 表示已到末尾。

### `context`

某条记录前后的上下文，用来在预览里直接显示搜索命中的位置。

参数：`record_id`（必填，来自搜索结果）、`session_id`（可选，给了就核对：记录不属于这个会话时按找不到处理）、`before`（0～100，默认 3）、`after`（0～100，默认 8）、`max_chars`、`tool_chars`（同 `read`）。

`record_id` 里编的是这台机器索引里的内部会话号，换一台机器可能对上别的会话，所以要跨机器查找时一定带上 `session_id`。0.2.0 及更早的服务端不做这个核对，客户端只接受 `session_id` 对得上的结果。

回复是一个 `page`，记录按时间顺序排列。字符预算先给命中的那条，再给用户和助手的记录、最后给工具记录，同一类里离命中近的先给；两端一个字也没分到的记录不返回。`offset` 是这批记录在会话中的起始位置，和 `read` 一样跨文件计数（Codex 拆成多个文件的会话），`next_offset` 指向这批之后的记录，可以接着用 `read` 读。0.2.0 及更早的服务端 `next_offset` 恒为 `null`，客户端按 `offset` 加记录数补上。

旧版服务端会回 `unsupported`，客户端此时退回用 `read` 显示会话开头。

## broker 消息（Unix socket，本机）

broker 接受连接后同样先写魔数，帧格式相同，不压缩。只接受同一用户的连接（socket 权限 0600，并校验对端 uid）。

CLI → broker：

| 消息 | 说明 |
|---|---|
| `{"t":"broker_hello","protocol":1,"build_id":"…","pid":123}` | 第一帧，5 秒内必须发出 |
| `{"t":"forward","id":1,"machine":{…},"op":{…},"timeout_ms":5000}` | 在指定机器上执行操作。`machine` 是完整的机器描述（id 和传输方式），broker 按它复用连接，不读配置文件 |
| `{"t":"req","id":2,"op":{"kind":"broker_status"}}` | 查询 broker 状态；`ping` 也可以直接发给 broker |
| `{"t":"cancel","id":1}` | 取消，向远端转发取消 |
| `{"t":"retire"}` | 继续服务，但在没有任何客户端连接时退出 |
| `{"t":"shutdown"}` | 立即退出 |

broker → CLI：`broker_hello{protocol, build_id, pid}`，之后是 `ok` / `err`，含义与远端相同。`ok` 里的 `took_ms` 是远端处理时间，`wire_bytes` 是远端回复帧在 SSH 上的大小。

版本替换：CLI 发现 broker 的 `build_id` 和自己不同但协议号相同时，本次照常使用它并发送 `retire`，旧 broker 在空闲时退出，下次调用再启动新版本，不会打断其他终端正在进行的请求。协议号不同时发送 `shutdown` 并立即替换。

## 连接可靠性（broker 行为）

所有操作都是只读的，因此 broker 可以安全地换一条连接重试一次：

- 请求途中连接断开（远端重启、重新安装），在截止时间内换新连接重发。
- 连接空闲超过 `probe_after_idle`（默认 20 秒）后，下一个请求发出时同时发一个 `ping`。`ping` 在 `min(2 秒, 剩余时间 / 2)`（至少 1 秒）内没有回复，就认定连接已经僵死（例如笔记本休眠唤醒后），丢弃它并换新连接重发请求。
- 请求超时且这条连接在整个等待期间没有收到任何字节时，不立刻丢弃：慢链路上大响应本来就可能超时，重连反而更慢。broker 在后台再发一个 `ping`（5 秒），确实没有回复才丢弃这条连接。

## 字符预算

`read` 的 `max_chars` 是整页共享的预算，按记录顺序分配。超出预算的记录保留元数据，正文被截断或清空，`truncated` 为 `true`。`next_offset` 指向本页之后的记录，被截断的正文不会在下一页重发；需要更多正文时用更大的 `max_chars` 或更小的 `limit` 重读。
