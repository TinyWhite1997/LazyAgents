# M0 Spike Report — LazyAgents

**作者**：Software Architect ｜ **日期**：2026-05-31 ｜ **关联**：[WEK-14](mention://issue/3c5705eb-dd8b-420b-ad84-d43cc82dd7c9) / [WEK-6](mention://issue/4827ec56-7d41-42f6-bda9-db53e1a6eb00)

## Scope

承接架构 §14 M0 与 PRD §6.5，本 spike 在第 1-2 周打通核心路径的关键不确定性：

1. **JSON-RPC 协议**能否承载 daemon ↔ client 的会话生命周期（initialize / sessions.create / sessions.attach / sessions.write / session.output）。
2. **PTY 抽象**在 Linux 上能否稳定 spawn / read / write / resize / signal / 看到 EOF。
3. **Adapter 抽象**能否独立于 IPC / SQLite / PTY，做到 trait 纯粹（§4.1）。
4. **闭关 TUI 不杀任务**这一关键不变量是否真正成立（client 关闭，PTY 不被一起带走）。

## 执行证据

| 子任务 | 验证形式 | 结果 |
| --- | --- | --- |
| [WEK-11 / M0.1](mention://issue/744aa95a-1d8e-4bc6-924c-f0c348ee2f09) | `cargo test -p la-pty` (5 smoke) | ✅ 本地 Linux 5/5 绿 |
| [WEK-12 / M0.2](mention://issue/96138202-5ce8-48b5-a350-03ab6125c5d0) | `cargo test -p la-proto -p la-ipc`（≥30 单元 + round-trip） | ✅ 本地 Linux 全绿 |
| [WEK-13 / M0.3](mention://issue/9de6530d-0224-41c6-9d27-33eae60975c6) | `cargo test -p la-adapter` + `LA_RUN_CLAUDE_E2E` 真实 CLI 实测 | ✅ probe/spawn_spec/encode/stop 通过 |
| [WEK-14 / M0.4](mention://issue/3c5705eb-dd8b-420b-ad84-d43cc82dd7c9) | `cargo test -p m0-smoke`：real la-pty + real la-proto + real la-ipc + real la-adapter + cat 后端 → write 后看到回声、drop client 后 PTY 仍 alive | ✅ Linux 通过 |

集成测试位于 `integration/m0-smoke/tests/m0_smoke.rs`，使用 `tokio::io::duplex` 作为 daemon ↔ client 的本地 transport，通过 `la-ipc` 的真实 length-prefix framing 编解码 `la-proto` JSON-RPC 消息，PTY 字节通过 `session.output` 推回客户端，最后通过 `drop(conn)` 模拟 client 退出并断言 daemon 端 `PtyChild.pid()` 仍存在。

## PRD §6.5 开放问题的 M0 答复

| 问题 | 状态 | M0 结论 |
| --- | --- | --- |
| 后端 JSON 模式是否稳定？ | 部分验证 | Claude 单次 prompt → 回复（`LA_RUN_CLAUDE_E2E`）通过；其余后端等 M2 接入时探。M0 不强依赖任何后端的 JSON 输出。 |
| daemon ↔ client 协议选型？ | 已 ADR-001 决策 | JSON-RPC 2.0 over length-prefix framing；m0-smoke 已用真实通路跑通。 |
| 既有会话发现/导入策略？ | 推迟到 M2 | M0 不涉及。 |

## 跨平台兼容现状（仅 Linux 实测，Windows/macOS 设计兼容）

| 议题 | Linux 证据 | Windows ConPTY 设计期望 | macOS pty 设计期望 |
| --- | --- | --- | --- |
| Spawn / read / write | la-pty smoke + m0-smoke cat 回环 | ConPTY master read/write 已写好，未实测 | 与 Linux 同分支 |
| Resize | `MasterPty::resize` 成功 | `ResizePseudoConsole`；Windows 不发 `SIGWINCH` | 同 Linux |
| Signal | `killpg(SIGINT/SIGTERM/SIGKILL)` 验过 | `GenerateConsoleCtrlEvent` + `TerminateProcess` 已编译 | 同 Linux |
| EOF | drop slave 后 reader 见 EOF | 依赖 ConPTY 关闭 + 进程退出 | 同 Linux |
| Detach 语义 | m0-smoke `drop(conn)` 后 `pid()` 仍存在 | 与 Linux 相同（daemon 持 PTY，client 是订阅者） | 与 Linux 相同 |

WEK-5 范围决策（用户）：v1 阶段只在 Linux 验证；ConPTY 路径靠 cfg gate 编译保留、真实测试推迟到 M4。

## 已验证 / 推翻的假设

| 假设 | 状态 | 证据 |
| --- | --- | --- |
| length-prefix JSON-RPC 足够承载 M0 协议 | 验证 | `la-ipc::Connection` 在 m0-smoke 中跑了完整 5 调用 + 多 notification 流。 |
| Adapter 可以做到不依赖 IPC / SQLite / PTY | 验证 | `la-adapter` 的 `Cargo.toml` 仅 dev-deps 引 la-pty；其余实现路径无强依赖。 |
| client detach 不会杀掉 PTY 子进程 | 验证 | m0-smoke `drop(conn)` → 200ms 后 daemon `last_known_pid` 仍存在。 |
| 后端 JSON 模式应当是 attach/write 的硬依赖 | 推翻 | cat 后端、claude 真实 CLI 都跑通，仅靠 PTY 字节流 + base64 即可。 |
| daemon 拉子进程必须挂在 tmux 上（claude-squad 模型） | 推翻 | portable-pty + setsid 直接管即可，跨平台路径对称。 |

## 已知风险（带入 M1）

1. **背压**：m0-smoke 用 `try_recv` 简化驱动；M1 必须按架构 §3 实现 1 MiB 订阅缓冲 + `session.gap` 通知 + `sessions.replay`。
2. **多 client**：M0 只有单 client；M1 需要在 `Connection::split()` 之上挂事件总线 + 写权抢占。
3. **ConPTY ANSI 注入**：未实测；M1 客户端渲染必须用 `vte` parser 容忍光标查询 / OSC。
4. **PtyChild::reader 所有权拆分**：当前 m0-smoke 用 `std::mem::replace` 把 reader 换走是一个 hack；建议 M1 在 la-pty 暴露官方 `into_parts()` 把 PTY 拆成 `(handle, reader, writer)` 三元组。

## 后续

M0 关闭后即进入 M1（核心 TUI 与 daemon 骨架）。串行下一条：[WEK-15](mention://issue/dea88826-b298-499d-8bef-bac6a74fad9b) la-proto 完整 schema（在 M0.2 5 方法基础上补全 §3 全部方法）。
