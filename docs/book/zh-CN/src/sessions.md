# 会话（Sessions）

LazyAgents 中的**会话**是一次后端 CLI（claude、codex、opencode 或自定义 adapter）在 PTY 中的运行。PTY 的所有者是 daemon，TUI 只是一个查看器。这个分离正是会话能扛住终端关闭、SSH 掉线、`la` 重启的原因。

## 生命周期状态

每个会话处于以下六种状态之一：

| 状态 | 含义 | TUI 标记 |
|---|---|---|
| `starting` | PTY 已拉起，还没有输出。250 ms 静默或首个输出字节后自动晋升为 `running`。 | `●` |
| `running` | 正在收发输出。 | `●` |
| `waiting` | 距上一字节超过 2 秒；后端在空闲等你。只有 stdin 是 PTY 的（交互）会话能进入此状态。 | `⏸` |
| `exited` | 子进程已结束。脚本仍保留。 | `·` |
| `errored` | 预留给 adapter 层失败。当前尚未触发。 | `✗` |
| `archived` | 软删除。`sessions.list` 默认不显示，需显式请求才返回。 |（位于 Archived 桶） |

日常常见迁移：

- `starting → running`：agent 开始有输出。
- `running ↔ waiting`：agent 进入空闲（你输入时又回到 running）。
- `running → exited`：agent 执行了 `exit`、你杀了它，或它崩了。
- `exited → archived`：你按了 `a`，把它从活跃列表里清掉。

## 创建会话

### 在 TUI 里（v1 状态）

v1 的 Sessions tab 完成了导航、侧栏与所有 modal，**New-session 表单已端到端接入**：在项目上按 **`n`** 会打开一个真表单，可以挑 backend、敲提示词、勾选 worktree，按 **`Ctrl+Enter`** 即调 daemon 的 `sessions.create`。新会话会在下一次 ~2 s 刷新里出现在侧栏。

**Live attach 已接入：** 选中会话行按 **`Enter`** 会打开一个实时 PTY 面板，底层走 `sessions.attach { acquire_input: true }`，daemon 把 `session.output` 直接送进 transcript，你键入的每一个字符通过 `sessions.write` 回送到 session。

New-session 表单的字段：

| 字段 | 必填 | 说明 |
|---|---|---|
| Project | 是 | 取自侧栏当前选中的项目行 —— 打开 modal 前先把光标移到目标项目。 |
| Backend | 是 | 来自 daemon 健康探测中状态为 `Available` 的 backend；`←`/`→` 切换。 |
| Prompt | 是 | 启动时喂给 agent 的初始文本。多行可写：`Enter` 插入换行，`Ctrl+Enter` 创建。 |
| Worktree | 否（默认关） | `Space` 切换。开启则在 spawn 之前运行 `git worktree add -b la/session-<short-sid> <base>`。 |
| Args | 预留 | trait 已留位以便未来扩展，目前 modal 不暴露。 |

modal 内的快捷键：

| 键 | 行为 |
|---|---|
| `Tab` / `Shift+Tab` | 在 Backend → Prompt → Worktree 之间循环焦点。 |
| `←` / `→` | 在 Backend 字段移动 backend 光标。 |
| `Space` | 在 Worktree 字段切换 worktree 标志。 |
| `Ctrl+Enter` | 触发 `sessions.create`。 |
| `Enter` | 在 Prompt 字段插入换行；在其它字段等同 Confirm。 |
| `Esc` | 取消 —— 关闭 modal 并丢弃草稿。 |

校验失败（提示词为空、无可用 backend）会保留 modal 并在内部贴上错误提示，方便就地修复；daemon 拒绝则关闭 modal，并通过底部状态栏 toast 显示原因。

### 程式化（通过 daemon socket 的 JSON-RPC）

```json
{"jsonrpc":"2.0","id":1,"method":"sessions.create","params":{
  "project_dir": "/home/alice/code/myapp",
  "backend":     "claude",
  "worktree":    true,
  "prompt":      "Add a README about the build system."
}}
```

响应包含 `session_id`（UUID v7）、解析后的 `cwd`（`worktree: true` 时是 worktree 路径）、初始 PTY 尺寸（`32 × 120`）。返回时的状态始终是 `starting`。

## attach、detach、列表

| TUI 键 | 效果 |
|---|---|
| `j` / `k` / 方向键 | 移动会话列表光标。 |
| `Enter` | attach 到选中的会话（打开实时 PTY 面板；daemon 持有输入）。 |
| `Ctrl+B d` | detach 前缀 → `d` 退回侧栏（session 仍在 daemon 上跑）。`Ctrl+B Esc` 与 `Ctrl+B .` 同样工作。 |
| `Ctrl+B Ctrl+B` | 给 PTY 发字面量 `Ctrl+B`（0x02），方便依赖该键的 agent。 |
| 其它任何按键 | 作为 PTY 输入转发到 daemon —— 包括方向键、PgUp/PgDn、Home/End、功能键。当前没有本地滚动模式，面板由 agent 进程拥有。 |
| `q`（在侧栏中） | 退出 `la`。会话与 daemon 都还活着。 |

**detach vs 退出：** `Ctrl+B d` 仅释放你的查看器，daemon 立即放弃你的 `acquire_input` 所有权（走 `sessions.detach`）。退出 `la` 在此基础上额外关闭 TUI 进程。两者都不会停止会话。

**重新 attach：** 你再次打开 `la` 时，daemon 会把内存环形缓冲（每会话 2 MiB）中的内容在 attach 时回放，让你跟上 "现在"。超出这个范围的输出在持久脚本里（见下文），但不会自动回放。

## 输出存到哪里

LazyAgents 把每个 PTY 块 —— 你的输入、agent 的输出，甚至 adapter 事件 —— 都记入 SQLite。这套表结构既支持脚本查询，也支持任意连续区间的回放。

- 会话前 **8 MiB** 的块存在 `lad.sqlite` 的 `session_chunks` 表里。
- 超过之后，daemon **溢出到文件** `<state_dir>/sessions/<session_id>.log` —— 换行分隔 JSON，每行一块，payload 是 base64。溢出文件未压缩（cron 运行归档才压缩；会话脚本不压缩）。
- daemon 还维护**每会话 2 MiB 的内存环形缓冲**，供 attach 时快速回放。

溢出文件不建议手改。如果你想要 "保存我的脚本" 这种功能，请用 `sessions.replay` RPC。

## 回放

两种方式获取 daemon 已经保留的输出：

1. **重新 attach 回放（最常见）。** `sessions.attach` 带 `resume_from_seq: <last_seq>` 让 daemon 只回放比你上一次看到的更新的块，然后继续直播。TUI 在每次重连时都会这么做。
2. **显式回放。** `sessions.replay` 带 `{ session_id, from_seq, max_bytes? }`，把一段历史输出作为 `session.output` 通知排队。用于想抓取历史片段、又不想动直播光标的工具。

如果环形缓冲在你的查看器追上之前已经丢弃了部分字节，daemon 会发出 `session.gap` 通知，附带丢弃的 seq 范围与字节数。TUI 显示 "missed N bytes" —— 数据仍在脚本里，只是环形缓冲不能回放。

## 调整尺寸

会话默认 `32 行 × 120 列`。终端 resize 时，TUI 会重定它的视图，并（在未来版本中）调用 `sessions.resize` 把新尺寸推给 PTY。PTY 层在 Unix（`TIOCSWINSZ` + `SIGWINCH`）和 Windows（`ResizePseudoConsole`）上都完全支持；只有 daemon 一侧的 RPC dispatcher 在 v1 尚未接入。因此依赖 `SIGWINCH` 的子进程在 v1 内不会中途重绘。若需要改尺寸，建议重启会话。

## archive vs delete

两者都拒绝处理仍在活跃注册表中的会话 —— 必须先停掉它（`sessions.signal` 发 `TERM` 或 `KILL`，或让 agent 自行 `exit`）。

| | Archive（TUI `a`） | Delete |
|---|---|---|
| 从 SQLite 删除行 | 否 | 是（级联删脚本块） |
| 脚本块删除 | 否 | 是 |
| `.log` 溢出文件删除 | 否 | 否（在磁盘上变孤儿） |
| worktree 目录删除 | 是（best-effort） | 否 |
| worktree 分支删除 | 仅在分支没有超出 base 的提交时 | 否 |
| 可恢复 | 行仍在；恢复 UI 是 roadmap | 否 |

**首选 archive。** delete 是给你确实想清掉脚本时用的。v1 没有对孤儿溢出文件做 GC —— 如果 `sessions.delete` 掉了已经溢出的会话，请手动清理 `<state_dir>/sessions/<sid>.log`。

## 发现并导入已有会话

LazyAgents 能把你直接用 `claude`、`codex`、`opencode` 启动的会话呈现出来 —— 不会拷贝任何东西。discover walk 是只读的。

daemon 一侧完整接入（`adapters.discover` + `sessions.import`），TUI 在 `i` 键上发出 `ImportDiscovered` action —— 真正的导入浮层与上面 New-session 表单属于同一批 UI 工作。在那之前请通过 JSON-RPC 触发导入。

导入后，会话与原生 LazyAgents 会话并列展示，"resume"（计划在同一里程碑落地）时 daemon 会用对应后端的 resume 标志拉起一个全新的进程，指向原始脚本文件（LazyAgents 从不改写它）。

discover 的根路径（与覆盖方式）：

| 后端 | 默认路径 | 环境变量覆盖 |
|---|---|---|
| Claude | `~/.claude/projects/` | `CLAUDE_SESSIONS_DIR` |
| Codex | `~/.codex/sessions/` | `CODEX_SESSIONS_DIR` |
| OpenCode | `$XDG_DATA_HOME/opencode/sessions` | `OPENCODE_SESSIONS_DIR` |

已导入的行会被标记，TUI 会把它们置灰。重复导入是幂等的。完整的数据归属规则见 [`docs/data-ownership.md`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/data-ownership.md)。

## Hook

LazyAgents 会在 **worktree 会话**成功 spawn 后（即调用 `sessions.create` 时 `worktree: true`）检查 `<project_root>/.lazyagents/hooks/post-create.sh` 是否存在并可执行，若是则运行。hook 限时 60 秒，失败只算 advisory，不会中止会话。可用于按项目做初始化，例如写入 env 文件或预热缓存。

不带 worktree 的会话跳过 hook —— 没有逐会话目录可作用。

## UI 偏好（主题、紧凑、按键提示）

TUI 的 `[ui]` 段位于 `$XDG_CONFIG_HOME/lazyagents/config.toml`：

```toml
[ui]
theme = "auto"        # auto | dark | light
key_hints = "rich"    # rich | compact | hidden
compact = false
```

你可以手编辑文件，也可以在 TUI 内按键：

| 键 | 效果 |
|---|---|
| `T` | 循环切换主题：auto → dark → light |
| `H` | 循环切换 key_hints：rich → compact → hidden |
| `C` | 切换 compact 布局 |

改动会立即通过原子 rename 写回 `config.toml`。你手写的其它段（`[daemon]`、`[scheduler]`、`[adapters.*]`）会逐字保留。

## 备份

```sh
lad backup --output ./lad-snapshot.sqlite
```

使用 SQLite Online Backup API，daemon 运行中也能安全执行。快照是单一文件 —— 没有 WAL / SHM 边文件。复制到异地即可备份所有会话行、脚本块、cron、run 历史。（`<state_dir>/sessions/*.log` 的溢出文件与 worktree 目录不在快照内；如有需要请一并备份。）

## RPC 参考

对工具与脚本，daemon 通过长度前缀的 UDS / 命名管道讲 JSON-RPC 2.0。你会用到的会话相关方法：

- `sessions.list`、`sessions.create`、`sessions.attach`、`sessions.detach`
- `sessions.write`、`sessions.resize`、`sessions.signal`
- `sessions.archive`、`sessions.delete`
- `sessions.import`、`sessions.replay`
- `adapters.discover`
- `events.subscribe` 主题：`session.output`、`session.state`、`session.gap`

每个 params / result 的 JSON Schema 都签入了 [`docs/schema/`](https://github.com/TinyWhite1997/LazyAgents/tree/main/docs/schema)，在每个 PR 的 CI 中与 wire types 做 golden diff。
