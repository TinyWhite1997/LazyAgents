# Quickstart

**目标：** 从零到跑起第一个会话，5 分钟以内完成。

> **v1 状态。** v1 中 daemon（`lad`）完整可用 —— sessions、crons、worktree、adapter 集成都工作。TUI 的 New-session / attach UI 仍在接入中（modal 当前是占位实现）。v1 想 "现在就跑一个会话" 的话，你会直接对 daemon 讲 JSON-RPC；本章既展示 TUI 今天的样子，也展示你今天就能跑通会话的 daemon 路径。

## 开始之前

你应当已经：

1. 装好了 `la` 与 `lad`（`la --version` 能跑通）—— 见[安装](install.md)。
2. **至少装好并登录了一个后端 CLI。** LazyAgents 自己不做鉴权 —— 它驱动你已经在终端里登录过的那个 CLI。

| 后端 | 安装 | 登录 |
|---|---|---|
| Claude Code | <https://docs.claude.com/en/docs/claude-code> | `claude login` |
| OpenAI Codex | <https://developers.openai.com/codex/cli> | `codex login` |
| sst.dev OpenCode | <https://opencode.ai/docs/> | `opencode auth login` |

先在终端确认后端能跑：

```sh
claude --version    # 或 codex --version、opencode --version
```

如果 `--version` 通过但工具提示你没登录，请先登录。LazyAgents 在第 4 步会原样把这个错误抛出来。

## 1. 启动 TUI

```sh
la
```

完事。`la` 会检查 `lad` 是否已经在跑，没在跑就在一个 `setsid` detach 的子进程中跑 `lad daemonize`，让 daemon 在关终端后继续活着。底部状态栏告诉你走了哪条路径：

| 状态栏文本 | 含义 |
|---|---|
| `daemon @ <socket-path>` | daemon 已经在跑。你已连上。 |
| `spawned lad @ <socket-path>` | `la` 找到 `lad` 并刚刚启动。 |
| `no daemon (lad not on PATH); start with 'lad daemonize'` | `la` 找不到 `lad`。把它加到 `$PATH`，或者自己起 daemon。 |
| `no daemon (LAZYAGENTS_NO_AUTODAEMON set); expected at <path>` | 你关掉了 auto-spawn。自己起 `lad daemonize`。 |
| `daemon spawn failed: ...` | `lad` 找到了但起不来。错误文本在冒号后面。见[故障排查 → daemon 起不来](troubleshooting.md#daemon-起不来)。 |

## 2. v1 UX 注意事项

LazyAgents v1 提供完整的 daemon —— sessions、crons、worktree、adapter 全部可用 —— 但 **TUI 的 New-session 表单与 attach 视图仍在接入中**（modal 是占位）。当前 TUI 中：

- 在项目上按 `n` 会打开一个仅确认按键的占位 modal，尚不会拉起后端。
- 在会话行按 `Enter` 会导航但还不会把 PTY 流入面板。

因此 v1 的 quickstart 有两条路径，取决于你想看什么：

- **路径 A（看 TUI）：** 打开 `la`，浏览空工作区、Crons 编辑器、按键浮层（`?`）。外壳是真的；New-session 胶水还没接好。
- **路径 B（直接驱动 daemon）：** 通过 socket 推 JSON-RPC —— 会话会真正拉起、脚本会持久化、cron 会触发、worktree 会创建。这是下面要做的。

## 3. 直接通过 daemon 跑一个真实会话

接下来你将通过 Unix socket 讲 JSON-RPC。任何能发长度前缀帧的 JSON 工具都行；这里给一段 Python 一次性脚本：

```python
import json, os, socket, struct

sock_path = os.path.expandvars("$XDG_RUNTIME_DIR/lazyagents/lad-1.sock")
s = socket.socket(socket.AF_UNIX); s.connect(sock_path)

def send(msg):
    body = json.dumps(msg).encode()
    s.sendall(struct.pack(">I", len(body)) + body)

def recv():
    (n,) = struct.unpack(">I", s.recv(4))
    return json.loads(s.recv(n))

send({"jsonrpc":"2.0","id":1,"method":"initialize",
      "params":{"client":"quickstart","client_version":"0.0.1",
                "protocol_versions":["1"]}})
print(recv())

send({"jsonrpc":"2.0","id":2,"method":"sessions.create",
      "params":{"project_dir": os.path.expanduser("~/code/myapp"),
                "backend": "claude",
                "worktree": False,
                "prompt":  "Add a README about the build system."}})
print(recv())
```

响应携带 `session_id`。用 `events.subscribe` 主题 `session.output` 与 `sessions.attach` 带 `resume_from_seq: 0` 拿到实时 PTY 字节；完整方法列表见 [Sessions 章节](sessions.md#rpc-参考)。

## 4. "我关了终端" 测试

这就是 LazyAgents 的核心价值。用上面路径 B 拉起的会话：

```sh
# 关终端、登出、甚至重启。
# 回来。重新连 socket。
# sessions.list { include_archived: false } 仍能看到你的会话还在跑。
# sessions.attach { session_id, resume_from_seq: <last> } 会从你离开处继续流出，
# 用每会话 2 MiB 的内存环形缓冲回放。
```

离开期间错过的输出在 attach 时从环形缓冲回放。如果环形溢出了（你离开太久且会话很活跃），会有 `session.gap` 通知告诉你丢失的 seq 范围 —— 数据仍在持久脚本里。

## 5. 看它扛住重启

daemon **不会**在系统重启时自动拉起（没有装 systemd unit）。重启之后：

```sh
la
```

`la` 会自动拉起 `lad`，后者从 SQLite 把状态读回来。重启时 PTY 子进程被杀掉的活跃会话会被标记为 `exited`；脚本仍保留。

## 这背后到底发生了什么

- `la` 通过权限 `0600` 的 Unix socket 连到 `lad`，路径在 `$XDG_RUNTIME_DIR/lazyagents/lad-1.sock`（用 `SO_PEERCRED` 校验 UID）。
- `lad` 通过 `portable-pty` 跨平台 PTY 抽象拉起你的后端 CLI，给每个输出块打上单调递增的序号，并写入 SQLite 支撑的脚本表。
- 你 detach 时 PTY 仍然活着 —— 拥有它的是 daemon，不是你的终端。

## 下一步

- 把同一个后端按计划跑：[Crons →](crons.md)
- 让 agent 在自己的分支上工作并评审 diff：[Worktree 评审 →](worktree.md)
- 把已经在 LazyAgents 外用 `claude`/`codex`/`opencode` 启动的会话纳入进来：[Sessions → 发现并导入](sessions.md#发现并导入已有会话)
- 配置主题与按键提示：[Sessions → UI 偏好](sessions.md#ui-偏好主题紧凑按键提示)
