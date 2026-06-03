# FAQ

## 通用

### LazyAgents *不是*什么？

- 不是后端本身。它驱动 `claude`、`codex`、`opencode` —— 你仍需要装并登录其中一个。
- 不是网络服务。没有 TCP 监听、没有遥测、没有上传。一切留在本机。
- 不是 tmux 的替代品。tmux 通用；LazyAgents 专做编码 agent，把 sessions、crons、worktree 作为一等概念。
- 不是多用户服务。socket 是 `0600` owner-only，daemon 在 accept 时做 `SO_PEERCRED` 强制。一个用户、一个 daemon。

### 有托管版吗？

没有。LazyAgents 设计上就是本地。

### 稳定吗？

是 v1，在 Linux 上完成验证。在 GA 之前，macOS 与 Windows 请按 Beta 质量对待。见[安装 → v1 平台范围](install.md)与[故障排查 → 平台说明](troubleshooting.md#平台说明)。

## 安装与更新

### 为什么没 Scoop bucket？

cargo-dist 0.32（我们用来打 release）原生支持 `shell`、`powershell`、`npm`、`homebrew`、`msi` 安装器 —— 没有一等的 Scoop 生成器。要干净发 Scoop manifest，需要自定义 publish job 或第三方工具，超过 v1 范围。Windows 上请用 PowerShell 安装器。Scoop 作为 follow-up 跟踪。

### 为什么 Homebrew tap 装不上？

每次 release 都生成了 Homebrew formula，但 `TinyWhite1997/homebrew-tap` 仓库还没发布。在那之前请用 `install.sh`。

### 会自动更新吗？

不会。`la --check-update` 查 GitHub Releases 告诉你有没有新版 —— 用安装器重跑一次升级。cargo-dist 的 self-updater 明确关闭（`install-updater = false`）。

### 怎么校验我下载的二进制？

```sh
gh attestation verify ./<artifact> --repo TinyWhite1997/LazyAgents
```

发布用 sigstore 支撑的 GitHub Artifact Attestations 签名。没有单独的 cosign 签名要校验 —— attestation *就是* sigstore 记录。

## 会话

### 重启后会话还在吗？

daemon 在系统重启时不会自动拉起（没装 systemd unit）。重启后跑 `la`，它自动起 `lad`，从 SQLite 重新播种。重启时 PTY 子进程被杀的活跃会话标记为 `exited`；脚本保留。

想让 daemon 自己回来的话，写一个 user systemd unit 调 `lad start` —— 在 roadmap 上但 v1 不带。

### 关终端后会话会怎样？

什么都不会。PTY 子进程归 `lad` 所有，不是你的终端。退 `la`（或终端死掉）只是 detach 查看器。

### 能从另一台机器 attach 吗？

不能。v1 仅本地。daemon 绑 Unix socket / 命名管道并做 `SO_PEERCRED` UID 检查，没有网络端点。

### 两个 `la` 能同时盯同一个会话吗？

能。多个订阅者各自从 daemon bus fan-out `session.output` 通知。可以一边 "watch" 一边 attach。

### 我的数据到底在哪？

| 项 | 路径（Linux 默认） |
|---|---|
| SQLite db | `~/.local/share/lazyagents/lad.sqlite` |
| 溢出脚本 | `~/.local/share/lazyagents/sessions/<sid>.log` |
| Worktree | `~/.local/share/lazyagents/worktrees/<project>/<sid>/` |
| Cron run 归档 | `~/.local/share/lazyagents/runs/archive/<yyyymm>.jsonl.zst` |
| UI 配置 | `~/.config/lazyagents/config.toml` |
| Socket | `/run/user/<uid>/lazyagents/lad-1.sock` |

用 `LAZYAGENTS_DATA_DIR`、`LAZYAGENTS_CONFIG_HOME`、`LAZYAGENTS_RUNTIME_DIR` 改动位置。

### 能导入已经用 `claude`/`codex`/`opencode` 起的会话吗？

能 —— 只读。discover walk 把它们呈现出来；在导入浮层里按 `i` 导入。LazyAgents 只存指针（`external_path`），从不复制。见 [Sessions → 发现并导入](sessions.md#发现并导入已有会话) 与 [`docs/data-ownership.md`](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/data-ownership.md)。

### 为什么不能 `sessions.delete` 一个正在跑的会话？

刻意如此 —— `delete` 在 SQLite 里是硬性 CASCADE。dispatcher 在会话仍在活跃注册表（状态 `starting`、`running`、`waiting`）时拒绝。先停掉它（`sessions.signal` 发 `TERM` 或 `KILL`，或让 agent `exit`），再 delete。

### `sessions.delete` 之后 `sessions/` 里的脚本文件会清吗？

v1 不会。`sessions.delete` 删除行并 CASCADE 删 `session_chunks` 行，但溢出的 `.log` 文件留在磁盘上变孤儿。在意磁盘的话手动清。

## Crons

### 比 bash + crontab 强在哪儿？

- LazyAgents 理解带 DST take-first 语义的 IANA 时区（见[Crons → 时区与 DST](crons.md#时区与-dst)）。
- 每次触发拉起一个真正的 LazyAgents 会话 —— 同样的脚本、attach、archive、worktree 集成。
- run 受配额闸门（最大并发、每日上限、每日 cost 预算），连续失败后自动暂停。
- catch-up 策略处理 "daemon 没在" 的情况，不会在唤醒时洪泛。

如果都不需要，经典 `cron` + shell 脚本更简单。

### 为什么启用 cron 要两下？

防呆。第一次调用返回一个 5 分钟有效的确认 token + 摘要（未来 5 次触发、每日 run 估算、预算影响）；第二次带上 token。改任何敏感字段（backend、schedule、prompt、args、budget）会**自动 disable** 并使待用 token 失效。TUI 把 round-trip 藏在一个 `Space` 后面，但这道闸门防止恶意编辑从 save 直接到 live。

### 能在 cron prompt 里放凭据吗？

别。prompt 以明文存在 SQLite（不加密、不按 secret 处理）。LazyAgents 刻意没用 `SecretString` 包它，因为 daemon 需要 diff 与持久化 prompt 文本。

### 为什么我 cron 起的会话是 "非交互"？

cron 触发的会话默认非交互 —— 要输入需要你显式 attach。这是安全选择（`docs/security.md`）：无人值守的触发若开放可写 stdin，agent 输出里的 prompt 注入就可能影响后续会话。

## Worktree

### LazyAgents 是原生说 git 还是 shell 调？

shell 调 `git` 二进制。没有 libgit2。要求 git ≥ 2.20，以便英文错误消息匹配可预测（分类器按字符串模式做）。

### 为什么 `worktree.discard` 要 `confirmed: true`？

fail-closed 默认。不知道这字段的旧客户端会拿 `WORKTREE_DISCARD_UNCONFIRMED`，而不是静默扔掉你的工作。

### TUI 里能 `--amend` 吗？

v1 不能。`worktree.commit` 只支持 `message` 与 `allow_empty`。amend 一个 agent 正在跑的分支是 footgun。

### archive 会话后 worktree 怎么办？

worktree 目录被移除。如果分支有超出 `base_branch` 的 commit，分支会保留，避免你丢工作。后台 sweep 在保留 TTL（默认 7 天）后清掉两者。

`sessions.delete` **不**触碰 worktree —— 设计上，因为 "delete" 在 SQLite 里就是 CASCADE，我们不想让它静默销毁未合并的 commit。

## Adapters

### 怎么加新 adapter？

`AgentAdapter` trait 在 `crates/la-adapter/src/lib.rs`。新 adapter 需要：`descriptor`、`probe`、`spawn_spec`、`encode_user_input`、`graceful_stop`，可选 `discover` 与 `parse_chunk`。看 `crates/la-adapter/src/{claude,codex,opencode}.rs` 作为可工作的样例。adapter 是纯的，不需要懂 IPC 或存储。

### LazyAgents 会替我登录吗？

不会，永远不会。鉴权由后端 CLI 拥有。你的 `claude` 在终端能用，LazyAgents 里就能用。

### 能按后端做限流吗？

cron 有单 cron 与全局配额（最大并发、每日上限、每日 USD 预算）。v1 没有按后端/按 API 的全局限流器 —— 需要的话提 issue。

## 隐私与安全

### LazyAgents 上网吗？

只在你的显式动作下：
- `la --check-update` 访问 GitHub Releases。
- release 安装器从 GitHub 下载。
- 你的后端 CLI（claude / codex / opencode）与各自 provider 通信 —— 这在 LazyAgents 之外。

没有遥测、没有使用统计。

### 同机器其他用户能读我的会话吗？

不能，但有附注：
- runtime dir 是 `0700`，socket 是 `0600`，daemon 在 accept 时检查 `SO_PEERCRED.uid == geteuid()`。其它 OS 用户连不上。
- SQLite 文件权限随你的 umask —— 通常是 `0600`。任何能读 `~/.local/share/lazyagents/lad.sqlite` 的人都能读你的脚本。别让别人共享你的 home。
- worktree 目录走标准 umask。同样规则。

### 那 root 呢？

root 能读任何东西。LazyAgents 不试图防范它。

### cron 定义里的 prompt 加密了吗？

没有。明文存 SQLite。当作 shell 脚本里的注释那样对待 —— 别放凭据。

## 可观测性

### 有结构化日志吗？

v1 没有。日志是 stderr 上的纯文本，由 `LAZYAGENTS_LOG` 控制（与 `RUST_LOG` 同语法）。JSON 日志在未来版本规划中。

### 有 Prometheus endpoint 吗？

`lad metrics` 是一个 stub，退出时打印 "not yet implemented"。真正的 metrics 面在 roadmap 上。

### 健康事件在哪？

daemon 广播 `daemon.health` 通知主题；通过 `events.subscribe` 订阅。它带每个注册 adapter 的周期探测结果，TUI 能实时显示 "claude OK、codex unauthenticated"。

## 出问题时

### 在哪儿提 bug？

<https://github.com/TinyWhite1997/LazyAgents/issues> —— 请附 `lad doctor` 输出、`la --version` / `lad --version`、能附就附 `LAZYAGENTS_LOG=debug` 抓取。
