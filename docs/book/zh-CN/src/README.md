# LazyAgents

> 让你的编码 agent 跨越重启、重连与 SSH 掉线后依然在线 —— 不必把笔记本钉在某个 tmux 会话上。

LazyAgents 由一个长驻本地的 **daemon**（`lad`）和一个轻量级 **TUI**（`la`）组成，专门用来托管无人值守的编码 agent —— Claude Code、OpenAI Codex、sst.dev OpenCode。会话不会因终端关闭而结束，cron 可以在你睡觉时跑它们，内置的 worktree 评审让你随时 stage 并 commit agent 产出的改动。

## 你将得到什么

- **持久化会话。** 断开、登出、重启都不会丢失。重新连上就能接着 agent 上次的位置干活。
- **定时运行。** 完整的 cron 实现，支持 IANA 时区与 DST 感知 —— 适合凌晨重构、早上代码评审。
- **每个会话一个 git worktree。** 每个 agent 拥有独立分支与工作目录。在 TUI 内 stage / unstage / discard / commit hunk。
- **三个开箱即用的后端。** `claude`、`codex`、`opencode`。登录由你掌控。
- **完全本地。** 没有网络监听端口、没有遥测、没有上传。你的会话和数据始终留在本机。

## 适合谁

你已经在用 `claude`、`codex` 或 `opencode` 跑得足够久，盯着一个终端窗口已经成为负担。你希望 agent 在你离开后继续工作 —— 也希望有一个统一的地方查看跨项目的所有产出。

## 本次发布范围（v1）

LazyAgents 仍在持续开发。**v1 仅在 Linux 上完成端到端验证。** 代码可在 macOS / Windows 上编译，发布流水线也会产出全部 5 个目标平台的二进制，但跨平台冒烟测试是 GA 之前的待办项。如果你在 macOS 或 Windows 上使用，请预期会有粗糙的地方 —— 已知问题见[故障排查](troubleshooting.md)。

## 选一条路径

- **第一次来？** 先看[安装](install.md)，再看 [Quickstart](quickstart.md)。5 分钟内就能跑起一个会话。
- **已经装好了？** 直接进 [Sessions](sessions.md) 或 [Crons](crons.md)。
- **遇到问题？** [故障排查](troubleshooting.md) 与 [FAQ](faq.md)。

## 项目链接

- 源码：<https://github.com/TinyWhite1997/LazyAgents>
- 发布（二进制）：<https://github.com/TinyWhite1997/LazyAgents/releases>
- 许可证：MIT
