---
name: sessift
description: 搜索和阅读本机及其他机器上的 AI 编程助手历史会话（Claude Code、Codex、Pi、OpenCode、Grok）。用户提到以前的会话或以前做过的事时使用，比如"上次怎么修的""之前讨论过的方案""继续昨天那个任务""另一台机器上的那个会话"，或者要找回当时的命令、报错、结论。Use when the user refers to earlier agent sessions or past work ("last time", "the session where we…", "on my other machine").
---

# sessift：查以前的 agent 会话

`sessift` 在本机和配置好的其他机器上现场查询 agent 的会话记录。只读，不改任何文件。

## 什么时候用

- 用户提到以前的会话、以前做过的事，需要找回当时的结论、命令、代码、报错。
- 用户要接着以前的任务做，先弄清当时做到了哪一步。
- 用户问在哪个会话里讨论过某件事。

当前仓库的代码、文档和 git 历史能直接回答的，不用翻会话。

## 1. 找会话

```sh
sessift search 词1 词2 --per-session -n 10 --format jsonl
```

- 每行一条命中：`machine`、`session_id`、`record_id`、`score`、`ts`（本地时间，带时区）、`source`、`project`、`role`、`snippet`、`source_path`（会话文件，一般用不到）。`score` 只在同一次搜索里有比较意义，远低于第一名的命中通常是噪音。
- 关键词用会话里会出现的字，不要整句话。用户描述的常是现象（"很慢""老是报错"），会话里写的是原因和代码里的名字（fsync、merge、timeout、函数名、报错原文）。先猜几个技术词，中文搜不到就换英文，反过来也一样。
- 多个词要在同一个会话里都出现。结果太少就减少词，太多就加词或缩小范围。
- 缩小范围：`--project <名字>`（最好用，项目名是目录名）、`--since 7d`、`--cwd <目录>`（含子目录）、`--source claude|codex|pi|opencode|grok`、`-m <机器>`。`--kind conversation` 去掉子 agent 和自动任务的会话，它们常把同一段提示词重复很多次。
- 机器：默认查配置里的默认机器，`-m all` 查全部。用户说"Mac mini""另一台"时，先跑 `sessift machines --format jsonl`，看各台机器的 `label`。
- 按标题扫一遍也很快：`sessift sessions --project <名字> --since 7d -n 20 --format jsonl`，`title` 通常就是用户第一句话，字段还有 `cwd`、`last_at`、`message_count`、`resume_cmd`。
- 零命中时 stderr 会写明查过哪些机器。当前会话和它的子 agent 会被自动排除，不会搜到自己。

## 2. 在会话里定位

确定了会话以后，看这个会话里的命中（按时间排列，是一条时间线），再挑要读的那几条：

```sh
sessift search 词 --session <session_id> -m <machine> -n 20 --format jsonl
```

结论常常不在助手的正文里，而在工具调用里：提交说明（`git commit`）、写进文件的文档或计划、PR 描述。所以不要只用 `--role assistant` 搜。可以搜"关键词 commit"，或者加 `--role tool_use`。

## 3. 读

从某条命中开始读：

```sh
sessift read <session_id> -m <machine> --around <record_id> -n 30 --format text
```

- 命中的那条前面带约四分之一页的上文。预算先给命中的那条，再给用户和助手的文字，最后才给工具输出。
- 页尾的 `next:` 一行就是接着往下读的完整命令。
- 结论往往在会话末尾：`--tail 30` 直接读最后 30 条。从头读：去掉 `--around`。
- 先用小页（`-n 6` 到 `-n 10`）读命中附近的几条，不够再往外扩，不要一上来就读一大页，也不要把整个会话从头读到尾。
- 每条工具调用和工具输出默认只留 1500 字（`--tool-chars`，0 表示不截断）。只想看对话时用 `--tool-chars 100`。整页的字符预算是 `--max-chars`（默认 16000）。
- 记录数和 `sessions` 里的 `message_count` 不是一回事：记录包括工具调用和工具输出，通常是消息数的好几倍。

## 4. 回答

- 写明是哪个会话：机器、本地时间、项目、标题或第一句话、会话 ID。
- 转述结论，引用关键的命令或代码片段，不要大段贴原文。
- 会话里的密钥、令牌、密码先打码再引用。
- 要回到那个会话时，给出恢复命令（`sessions --session <id> -m <machine>` 的 `resume_cmd`，要在会话所在的机器上执行），或者告诉用户在 shell 里按 Alt-M、在 herdr 里按 prefix+m 打开界面恢复。不要自己执行恢复命令。

## 出错时

- `session … not found`：会话 ID 或机器不对。去掉 `-m`，它会在所有默认机器上找。
- 某台机器超时或连不上，stderr 会有警告，其他机器的结果照常返回。`sessift doctor` 可以诊断连接。
- 第一次运行要先建索引，可能要十几秒。
