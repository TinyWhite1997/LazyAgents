# 适配器（Adapters）

**adapter** 是一段很薄的 Rust shim，教 LazyAgents 怎么跟某个后端 CLI 沟通。它干 5 件事：

1. **描述**后端（`id`、默认可执行文件名）。
2. **探测**可执行文件（装了吗？登录了吗？）。
3. **构建 spawn spec**（用哪些 argv 与 stdin 模式，取决于交互启动还是单次 prompt）。
4. **编码用户输入**（后端把哪个字节当 "提交" —— claude 是 `\r`，codex 与 opencode 是 `\n`）。
5. **发现磁盘上的现有会话**，方便导入。

adapter 是纯代码：不持有 PTY、不写 SQLite、不碰 IPC。所以容易用假 CLI 二进制做单测，也容易新增。

## v1 自带的 adapter

| Adapter id | 包装 | 默认可执行 |
|---|---|---|
| `claude` | Anthropic Claude Code | `claude` |
| `codex` | OpenAI Codex CLI | `codex` |
| `opencode` | sst.dev OpenCode | `opencode` |

默认可执行在 `$PATH` 上查找。v1 既没有 wire 层、也没有 `config.toml` 旋钮能把 adapter 指到非默认二进制 —— adapter 的 `SpawnRequest::program_override` 字段存在于 Rust API（debug 版 `lad` 的仅测试用 `--test-shell-adapter` 标志会用），但 `sessions.create` 的 wire schema 尚未把它穿出来，daemon 也不读 `adapters.*.command` 配置。今天若要指向 beta build 或 wrapper 脚本，请在 `$PATH` 上把它软链到真二进制之前。持久化每后端配置是 follow-up。

## 鉴权：LazyAgents 从不替你登录

这是刻意为之。LazyAgents 不存后端凭据，不实现登录流程。鉴权由后端 CLI 自己负责 —— `claude login`、`codex login`、`opencode auth login`。CLI 在自己的配置目录（通常是 `~/.claude/`、`~/.codex/`、`~/.config/opencode/`）维护的 session 就是 LazyAgents 拉起 CLI 时继承的那个。

这意味着：你的 `claude` 在终端里能用，LazyAgents 里就能用。如果 `claude --version` 说你没登录，LazyAgents 也会这么说。

## 当 adapter 说 "unauthenticated"

每个 adapter 在启动时和按需都会做两阶段探测：

1. 跑 `<executable> --version`。扫描 stdout/stderr 里的鉴权关键词（"not logged in"、"please log in"、"unauthenticated"、"no credentials" 等）。
2. 必要时跑次级探测：`codex login status` 或 `opencode auth list`。

匹配到关键词时返回 `Unauthenticated { docs_url }`。TUI 会把这个错误连同对应文档站点的链接显示出来。`AdapterError::Unauthenticated` 格式化为 `unauthenticated; see <docs_url>`。

| Adapter | docs_url | 修复 |
|---|---|---|
| `claude` | <https://docs.claude.com/en/docs/claude-code> | `claude login` |
| `codex` | <https://developers.openai.com/codex/cli> | `codex login` |
| `opencode` | <https://opencode.ai/docs/> | `opencode auth login` |

在终端重新登录后，daemon 的下次探测就会看到。LazyAgents 这边没有缓存需要清。

探测是**保守**的：只有显式的 "no credentials" / "not logged in" 关键词才会被判为未鉴权。非零退出但没匹配到关键词仍然算 `Available`，因为有些 CLI 在与鉴权无关的原因（网络抖动、容器问题）下也返回非零，我们不想错误地让你再去登录一遍。

## 会话发现（`adapters.discover`）

这是把你 LazyAgents 之外启动的会话呈现出来的只读 walk。

**Params：**

```json
{
  "backend":      "claude" | "codex" | "opencode" | null,   // null = walk 全部
  "source_path":  null,    // 覆盖 adapter 的默认 sessions 根（测试/fixture 用）
  "project_root": null     // 按 cwd 过滤
}
```

**结果条目：**

```json
{
  "backend":          "claude",
  "external_id":      "<后端自己的 session id>",
  "external_path":    "/home/alice/.claude/projects/.../<uuid>.jsonl",
  "project_hint":     "/home/alice/code/myapp",
  "title_hint":       null,                  // opencode 会填；claude/codex 不填
  "created_at":       "2026-05-30T12:34:56Z",
  "already_imported": true                   // daemon 侧：(backend, external_id) 已有行
}
```

各 adapter 的发现根目录：

| Adapter | 默认根 | 环境变量覆盖 | 文件 glob |
|---|---|---|---|
| `claude` | `~/.claude/projects/` | `CLAUDE_SESSIONS_DIR` | `*.jsonl` |
| `codex` | `~/.codex/sessions/` | `CODEX_SESSIONS_DIR` | `*.jsonl`、`*.json`（兼容旧平铺布局） |
| `opencode` | `$XDG_DATA_HOME/opencode/sessions` | `OPENCODE_SESSIONS_DIR` | `*.json`、`*.jsonl` |

walk 受上述 glob 边界限制；不追根目录之外的 symlink，也不改写/复制任何东西。完整数据归属规则见 [`docs/data-ownership.md`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/data-ownership.md)。

## 导入发现到的会话

> **v1 状态。** daemon 一侧 `adapters.discover` 与 `sessions.import` 完整接入（`(backend, external_id)` 唯一约束由迁移 `0003_session_external_reference.sql` 强制）。**TUI 导入浮层仍未接入** —— 今天按 `i` 只会翻转 TUI mock `SessionSource` 上的一个标志位，并不会调用 `sessions.import`。v1 真正要导入的话，通过 IPC socket 调这对 RPC。

### v1 路径：JSON-RPC

```json
{"jsonrpc":"2.0","id":1,"method":"adapters.discover","params":{"backend":"claude"}}
{"jsonrpc":"2.0","id":2,"method":"sessions.import","params":{
  "backend":      "claude",
  "external_ids": ["<discover 结果里的 external_id>"]
}}
```

`external_ids` 是数组 —— 一次传一个或多个。省略它会导入 adapter 当前 discover 到的所有会话；未知 id 静默丢弃，避免过期快照卡住调用。

导入 handler 创建一行 `origin = "import"` 的 LazyAgents 行：

- 新生成的 `session_id`（UUID v7），
- `external_id` 取后端的 id，
- `external_path` 指向原始脚本文件（LazyAgents 只读它），
- 从后端首行元数据取 `created_at`、`title`、`project_hint`。

对同一个 `(backend, external_id)` 重复导入是 no-op —— 这一约束由 schema 保证（迁移 `0003_session_external_reference.sql`）。

TUI 导入浮层落地后，`i` 键会 round-trip 到同一对 RPC；在那之前它只操作本地 mock 状态。

## resume 一条导入的会话

resume 在 roadmap 上，v1 **尚未接入**。落地时不会接管后端原先的 PTY（早没了）—— daemon 会用后端自己的 resume 标志拉起一个全新进程，指向你之前发现的 `external_path`。后端按手动 `claude --resume` 那样读自己的脚本文件，LazyAgents 把新 PTY 的字节并行记到自己的脚本里。

计划中的 resume 调用：

| Adapter | resume 调用（计划） |
|---|---|
| `claude` | `claude --resume <external_id>` |
| `codex` | `codex resume <external_id>` |
| `opencode` | `opencode run --session <external_id>` / `--continue` |

导入会话将遵循后端自己的保留规则 —— 后端如果裁剪或轮转了那个文件，对应行的 resume 也会同时失效。

## 各 adapter 备注

### `claude`（Anthropic Claude Code）

- 非交互标志：`--print`。
- TUI 提交字节：`\r`。
- 优雅停止：in-band `/exit\r` → SIGTERM → SIGKILL，带有界等待。
- 发现布局：`~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`。首行 JSON 记录携带 `session_id` / `sessionId` / `id`、`cwd` / `workingDir`、`timestamp` / `created_at`。
- claude 不暴露 `title_hint`。

### `codex`（OpenAI Codex CLI）

- 针对 `codex-cli 0.135.0` 设计。
- 非交互标志：`exec --json --cd <cwd> <prompt>`。
- TUI 提交字节：`\n`。
- 优雅停止：仅信号（SIGTERM → 等 → SIGKILL）。codex 没有跨版本稳定的 in-band 退出命令。
- 发现布局：嵌套 `YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`（当前）或平铺 `*.json`（旧）。walk 同时容忍两种。
- 首行 JSON 记录有 `kind: "session_meta"` 包裹 payload。
- codex 不暴露 `title_hint`。

### `opencode`（sst.dev OpenCode）

- 针对 `opencode 1.2.15` 设计。
- 非交互标志：`run --format json [--dir <cwd>] <prompt>`。
- TUI 提交字节：`\n`。
- 优雅停止：仅信号。
- 发现布局：平铺 `$XDG_DATA_HOME/opencode/sessions/*.json`（与 `.jsonl`）。同时容忍纯 payload 与嵌套信封（`meta` / `session` / `payload` 键）两种形态。
- `title_hint` 来自 `meta.title` —— 这是唯一暴露 session 标题的 adapter。

## 通过 IPC 触发 adapter 发现

工具或脚本：

```json
{"jsonrpc":"2.0","id":1,"method":"adapters.discover","params":{"backend":"claude"}}
```

JSON Schema：
- params：[`docs/schema/adapters__discover.params.schema.json`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/schema/adapters__discover.params.schema.json)
- result：[`docs/schema/adapters__discover.result.schema.json`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/schema/adapters__discover.result.schema.json)

CI 中的 schema 检查会在两者之一与 wire types drift 时 fail —— 你在 schema 里看到的就是 daemon 接受的。
