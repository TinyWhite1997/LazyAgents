# 故障排查

bug 与粗糙点，按你最早会碰到的顺序排。这里没覆盖到你看见的现象的话，请提 issue：<https://github.com/TinyWhite1997/LazyAgents/issues>。

## `la` 起不来

### 状态栏出现 `daemon: missing` / `spawn-failed`

`la` 找不到或起不来 `lad` 二进制。

1. 检查 `lad` 是否在 `$PATH`：`which lad`。通过 cargo/script 安装时通常在 `~/.cargo/bin` 或 `~/.local/bin`。
2. `lad` 装在非标准位置时，`la` 先在自己二进制的同目录找，再走 `$PATH`。把两者软链或放到同一目录。
3. 跑 `lad doctor` 看 daemon 会用哪个 state dir / socket，确认 runtime dir 可写。
4. 直接跑 `lad start` 看 daemon 侧的启动错误，绕过 auto-spawn 吞掉 stderr 的行为。

### 哪儿设了 `LAZYAGENTS_NO_AUTODAEMON=1`

auto-spawn 被抑制。要么 unset，要么自己起 `lad daemonize`。

### socket 权限拒绝

runtime dir 是 `0700`，socket 文件是 `0600`，设计上就是 owner-only。如果你之前以另一个用户（如 `sudo`）跑了 `lad`，现在以自己的身份跑 `la`，那个 socket 就读不了。停掉 daemon、删掉 `$XDG_RUNTIME_DIR/lazyagents/lad-1.sock`、再跑。

## daemon 起不来

### Address already in use / socket 文件残留

上一个 `lad` 没清理 socket 就崩了。daemon 拒绝在一个看起来活着的文件上 bind。先确认没有 daemon 在跑：

```sh
pgrep -f 'lad start' || rm "$XDG_RUNTIME_DIR/lazyagents/lad-1.sock"
```

### 启动时无说明地崩

拉高日志级别：

```sh
LAZYAGENTS_LOG=debug lad start 2> /tmp/lad.log
```

日志是 stderr 上的纯文本（v1 没有 JSON 日志格式）。`LAZYAGENTS_LOG` 接受与 `RUST_LOG` 相同的语法 —— 例如 `LAZYAGENTS_LOG=la_daemon=trace,la_storage=debug`。

### SQLite 迁移报错

最常见原因是部分升级 —— 磁盘 schema 比二进制新。确认：

```sh
sqlite3 "$XDG_DATA_HOME/lazyagents/lad.sqlite" 'SELECT * FROM _sqlx_migrations;'
```

若是你刻意降级到旧 `lad`，db 也得跟着降级（或直接清掉：`rm $XDG_DATA_HOME/lazyagents/lad.sqlite*`）。LazyAgents 不会自动降级。

## 会话起不来

### `unauthenticated; see <docs_url>`

后端 CLI 因为没有可用凭据拒绝了 spawn。要么登录，要么在 daemon 环境里提供 API key（例如 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`）。登录可直接跑 CLI：

- `claude` → `claude login`
- `codex` → `codex login`
- `opencode` → `opencode auth login`

然后重建会话。注意：侧栏里单纯的 "not logged in" 提示**不会**阻止创建（CLI 可能仍有 API key）；只有 CLI 在 spawn 时自己拒绝才会出现这个错误。见[适配器 → 当 adapter 说 "unauthenticated"](adapters.md#当-adapter-说-unauthenticated)。

### spawn 时 `command not found`

adapter 期望后端 CLI 在 `$PATH`。装上（或在 `$PATH` 上把它软链到同名二进制之前）。v1 没有 `config.toml` 旋钮把 adapter 指到非默认路径；持久化的每后端配置项在 roadmap 上。

### 会话开始前 worktree 创建失败

你会看到 `WorktreeProvision` 错误。常见原因：

| 错误片段 | 修复 |
|---|---|
| `the git binary was not found on $PATH` | 装 git。 |
| `'la/session-...' is already used by worktree at ...` | 在项目仓库里跑 `git worktree prune`，再 `git branch -D la/session-<sid>`。 |
| `not a git repository` | 选一个实际是 git 仓库的项目根，或 `git init`。 |

daemon 对每个 git 子进程强制 30 秒超时。仓库因网络挂载或巨大历史而慢的时候可能踩中；把仓库挪到本地磁盘。

## 丢输出 / "gap" 提示

TUI 显示 `missed N bytes` 时，说明内存环形缓冲（每会话 2 MiB）在你的查看器追上之前溢出了。数据**仍在持久脚本里** —— 只是 live-replay 路径丢了。

恢复流程：

1. detach（Esc）。
2. 重新 attach。TUI 回放仍在环里的部分。
3. 如果你确实需要丢失的字节，脚本块在 `session_chunks`（≤ 8 MiB 总量）或 `<state_dir>/sessions/<sid>.log`（溢出，JSONL，payload 是 base64）。

## 平台说明

### Linux

v1 在这里完成验证。遇到 bug 请附 `lad doctor` 输出与 `LAZYAGENTS_LOG=debug` 抓取，提 issue。

### macOS

发布流水线产出 `x86_64-apple-darwin` 与 `aarch64-apple-darwin`，但 **macOS 冒烟测试是 GA 的发布闸门，不是 Beta 的**。理论上能工作的：PTY spawn/read/write/signal（`portable-pty` 的 Unix 路径与 Linux 共用）、worktree 评审、cron。

需要留意的：首次运行时的代码签名提示（二进制是 sigstore 证明，不是 Apple notarise），以及 `setsid` / 进程组行为的潜在差异。如有异常请附 `lad doctor` 输出与抓取，提 issue。

### Windows

发布流水线产出 `x86_64-pc-windows-msvc`。Windows ARM 在 v1 内刻意不支持。初期 spike 报告记下的已知问题：

- **EOF 上报延迟。** Unix PTY 在子进程退出后能立即报 EOF；Windows ConPTY 在 GitHub-hosted runner（也可能不止）上会让 reader 在子进程结束后还开着一阵。会话短暂看起来 "还在跑"。
- **`Signal::Interrupt` 可能杀不掉子进程。** `GenerateConsoleCtrlEvent(CTRL_C_EVENT)` 可能返回成功但 ConPTY 子进程并未实际退出。Ctrl-C 没停下 agent 时，跟上一个 `Signal::Kill`（TUI 的 "force kill" 路径）。
- **ConPTY 会发额外的 ANSI/OSC chatter**（光标位置查询、模式变更报告）。LazyAgents 的 VTE 解析器会静默吸收，不会出现在脚本里 —— 但这解释了为什么 Windows 上脚本回放可能比 Unix 多噪声。
- **Resize 不会 `SIGWINCH`**（Windows 没这个信号）。控制台应用需要轮询 `GetConsoleScreenBufferInfo`。某些 TUI agent 在 Windows 上 resize 后可能不干净重绘。

最低 Windows 版本：**10 build 1809**（最早带 ConPTY 的版本）。

## config 文件坏了

`config.toml` TOML 损坏时，TUI 仍用内存默认值跑，并**拒绝保存** UI 改动（写回会覆盖你已有的好内容）。修好 TOML 重启。启动时会看到一条 toast 提示保存被拒。

## "我后悔了，恢复这条 archive"

v1 没有 `sessions.unarchive` RPC。行仍在 SQLite，`state='archived'` 且 `archived_at` 已设置，`sessions.list { include_archived: true }` 能返回 —— 数据没丢；TUI 暂时还没暴露恢复动作。

修复落地之前若要手动解 archive，可改行：

```sh
sqlite3 "$XDG_DATA_HOME/lazyagents/lad.sqlite" \
  "UPDATE sessions SET state='exited', archived_at=NULL WHERE id='<sid>';"
```

（别在 daemon 跑着且该会话内存里仍标 archived 时这么做 —— 改完后重启 `lad`。）

## 崩溃 / panic

`lad` 会在 daemon state 目录下写 `crashes/<ts>.json`。文件包含 panic 位置 / 消息，以及最近 100 条 tracing 事件。你选择上报崩溃时，把这个文件附到 issue 即可。

## cron 没触发

按顺序排查：

1. cron 是不是**已启用**？在行上按 Space —— 必须是 enabled，不只是 saved。（改任何敏感字段会自动 disable。）
2. 跑 `R`（dry-run）—— 下次预测触发时间是不是你期望的？
3. 时区对不对？cron 存 IANA 名；检查 `Asia/Shanghai` 与 `UTC` 别搞错。
4. 调度时刻 daemon 是不是在线？不在的话由 `catchup_mode` 决定：`skip` 丢弃、`coalesce`（默认）唤醒时只发一次、`replay` 把全部错过都入队。
5. cron 自动暂停了吗？`consecutive_failures >= pause_on_consecutive_failures` 会关掉它。TUI 显示 "paused after N failures"。
6. 后端登录没？没登录 cron run 会以 `unauthenticated; see <docs>` 失败。

## 先看哪里

| 症状 | 先查 |
|---|---|
| `la` 一片空白 | 你的终端是不是 UTF-8 locale？有些模拟器默认非 UTF-8。 |
| 颜色错乱 | 在 `[ui]` 显式设 `theme = "dark"`（或 `"light"`），跳过自动检测。 |
| 文件明明改了 diff 却空 | 文件可能 > 5 MiB 或是二进制 —— 改用 `worktree.open_in_editor`。 |
| cron 触发了但没会话 | 看 `runs` 表 —— 行里有 `status`、`error_kind`、`error_detail`。 |

## 找帮助

- Issues：<https://github.com/TinyWhite1997/LazyAgents/issues>
- 提 issue 时请附：
  - `la --version` 与 `lad --version`
  - `lad doctor` 输出
  - 你的 OS / 终端 / Rust 工具链（如果是源码构建）
  - 如条件允许，附一段 `LAZYAGENTS_LOG=debug` 的失败抓取
