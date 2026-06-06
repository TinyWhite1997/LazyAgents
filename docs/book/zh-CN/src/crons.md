# 定时任务（Crons）

LazyAgents 内置一个完整的 cron 调度器。给它一个 cron 表达式、IANA 时区、后端、prompt —— 每次调度触发就拉起一个全新的会话。适合做凌晨重构、早间代码评审摘要、每小时链接检查 —— 凡是你过去会用 `systemd-timer` + shell 脚本拼出来的事。

## cron 表达式语法

LazyAgents 同时接受 **5 字段**与 **6 字段** cron 表达式。

| 格式 | 字段 | 示例 |
|---|---|---|
| 5 字段（经典） | `minute hour day-of-month month day-of-week` | `0 9 * * 1-5`（工作日 09:00） |
| 6 字段（含秒） | `second minute hour day-of-month month day-of-week` | `*/30 * * * * *`（每 30 秒） |

5 字段输入会被自动提升为 6 字段（前面补 `0`），所以经典表达式在匹配到的分钟的 `:00` 秒触发。

7 字段（含年）输入被**拒绝** —— 这是刻意为之，避免静默意外。

解析由 [`cron`](https://crates.io/crates/cron) crate（0.16）提供；完整 token 语法（`*/N`、区间、列表等）见该 crate 文档。

## 时区与 DST

每个 cron 存一个 IANA 时区名（如 `America/Los_Angeles`、`Asia/Shanghai`、`UTC`）。调度算术通过 `chrono-tz` 在该时区内完成；调度器内部以 UTC 存触发时间，展示时再转回 cron 自己的时区。

**Fall-back（不明确）的本地时刻：** LazyAgents 采用 **take-first** 策略。当一个本地 wall-clock 在 DST 回拨日出现两次（例如 11 月 1 日 `01:30 America/Los_Angeles` 同时对应 `08:30Z` 与 `09:30Z`），LazyAgents 只触发第一次。这符合大多数用户对 "每天一次的 wall-clock cron 一天触发一次" 的预期，避免重复扣费或重复副作用。

**Spring-forward（不存在）的本地时刻：** 按 `chrono-tz` / `cron` 默认行为跳过，使用下一个有效时刻。

完整论证见 [ADR-0002](https://github.com/TinyWhite1997/LazyAgents/blob/main/docs/adr/0002-cron-dst-fallback-take-first.md)。

## 创建 cron

> **v1 状态。** daemon 一侧的 cron 表面（`crons.upsert`、`crons.set_enabled`、`crons.run_now`、`crons.dry_run`、admission gate、scheduler、catch-up、归档）在 v1 完整接入 —— 下面描述的所有行为都是 cron 写入 SQLite 之后 `lad` 实际执行的事情。**TUI Crons tab 仍是 mock-backed**：编辑器、列表与 `Space` / `r` / `R` / `d` 都只在内存中的 `MockCronSource` 上操作，**还不会与 daemon 来回**。今天要创建、启用、触发真实 cron，请通过 IPC socket 调 `crons.*`；下面 TUI 键位表里描述的是 live-source wiring（UI 层已经写好的草稿）。

### v1 路径：JSON-RPC

```json
{"jsonrpc":"2.0","id":1,"method":"crons.upsert","params":{
  "name":      "nightly-refactor",
  "project_id":"<你的 project id>",
  "backend":    "claude",
  "spawn_args": [],
  "prompt":    "Run the nightly refactor checklist.",
  "cron_expr": "0 2 * * *",
  "tz":        "America/Los_Angeles"
}}
```

随后用 `crons.set_enabled { cron_id, enabled: true }` 启用。具体 wire 形态见下文 [启用 cron](#启用-cron) —— 注意 v1 启用路径没有 token 闸门、没有敏感字段自动 disable、也没有 IPC 层的 prompt 大小上限。

### TUI 编辑器（今日 mock-backed）

TUI 切到 **Crons** tab，按 **`n`** 打开编辑器。`Tab` 在字段间循环：

| 字段 | 说明 |
|---|---|
| Name | 可读名字。 |
| Backend | `claude`、`codex`、`opencode`... |
| Spawn args | 传给后端 CLI 的额外参数。 |
| Cron expr | 5 字段或 6 字段。 |
| Tz | IANA 名。 |
| Prompt | **不要**在里面放凭据 —— 见 [安全注意事项](#安全注意事项)。`cron_security` 里有 64 KiB 上限（`MAX_PROMPT_BYTES`），但 v1 的 IPC 边界尚未启用。 |
| Budget | 每日 USD 上限、每次最长运行时间、最大并发数等。 |

`Ctrl+S` 保存草稿。`Esc` 丢弃。**v1 中这些写入只落到 TUI 的 mock 源**；后续会换成 live 的 `IpcCronSource`，同样的键位将真正调 `crons.upsert`。

## Crons tab 键位

| 键 | 当前效果（对 mock） | 接入 live 源后的效果 |
|---|---|---|
| `j` / `k` / `↓` / `↑` | 移动光标 | 同 |
| `n` | 新建 cron 草稿 | 同；保存调 `crons.upsert` |
| `e` / `i` / `Enter` | 编辑高亮的 cron | 同 |
| `Space` | 翻转本地 enabled 标志 | `crons.set_enabled`（v1 单次调用；token 闸门将在 security helper 接到 RPC 后启用） |
| `r` | mock `trigger_now` | `crons.run_now` |
| `R` | 本地 dry-run 预览 | `crons.dry_run` |
| `d` | 从 mock 删除 | `crons.delete`（带确认弹窗） |
| `Ctrl+S` | 保存当前草稿 | 同；round-trip 到 `crons.upsert` |
| `Esc` | 取消草稿 | 同 |

在 live 源接入之前，**不要依赖 TUI 来调度真实工作** —— 在这里做的一切都是本地的，退 `la` 即丢。

## 启用 cron

cron 在 save 后不会自动 enabled —— `crons.upsert` 始终把它落成 disabled。v1 中启用一条 cron 只需一次 `crons.set_enabled`：

```json
{"jsonrpc":"2.0","id":3,"method":"crons.set_enabled","params":{
  "cron_id": "<upsert 返回的 id>",
  "enabled": true
}}
```

wire params 就是 `{cron_id, enabled}`；响应携带更新后的 cron 行。**v1 RPC 表面上当前没有任何启用期硬化**：没有确认 token、没有敏感字段 auto-disable、也没有 IPC 层 prompt 大小检查。daemon 自带一个 `cron_security` 模块（`crates/la-daemon/src/cron_security.rs`），里面有 5 分钟单次 token + summary helper、`SENSITIVE_CRON_FIELDS` 白名单（backend、args、prompt、schedule、timezone、runtime 限制、max-per-day、daily budget）、64 KiB `MAX_PROMPT_BYTES` 上限 —— 但**dispatcher 与 scheduler 今天都没调用任何一项**。请把这些都当作计划中的硬化，而非当前用户层行为。

v1 *今天确实生效*的保护：

- **新建 cron 默认 disabled**。`crons.upsert` 在 id 全新时把 `enabled = false`；必须显式 `crons.set_enabled` 才会启用。
- **更新会保留当前 `enabled` 位**。对一条已经 enabled 的 cron 做更新 —— 即便改 backend、prompt 或 schedule —— 它仍然 enabled，下次触发会用新值。v1 中没有自动 disable 这层安全网；脚本化做风险编辑时，请先 `set_enabled { enabled: false }`。

`cron_security` 接到 RPC 表面后，敏感字段 auto-disable 与 token round-trip 都会启用。在那之前，`crons.set_enabled` 的保护只有 "必须显式调用" 这一条。

## cron 触发时会做什么

调度器堆顶弹出下一个条目，executor 拿到 admission lock，评估单 cron 与全局配额（最大并发、每日上限、预算），插入一条 `runs` 行，然后：

1. 为 `backend` 解析 adapter。
2. 解析项目根。
3. 用 `spawn_args`、解析后的 cwd、预装的 prompt 调 `SessionManager::spawn`。
4. 把新的 `session_id` 写回 `runs` 行，状态置为 `running`。
5. 广播 `cron.fired` 通知（TUI 状态栏会闪一下）。
6. 起一个秒级轮询的 watcher，强制执行 `max_runtime_s`（超时发 SIGTERM）、失败时累加 `consecutive_failures`，达到 `pause_on_consecutive_failures` 时自动暂停 cron。

cron 会话默认**非交互** —— 输入所有权需要你显式 attach（在会话行按 `Enter`）。这是有意为之：cron 是无人值守流程，不是即时 shell。

## 追赶（catch-up）策略

若到点时 daemon 正在睡觉（笔记本挂起、daemon 崩溃、系统重启），唤醒后由每个 cron 的 `catchup_mode` 决定下一步：

| 模式 | 行为 |
|---|---|
| `skip` | 错过的触发静默丢弃。 |
| `coalesce`（默认） | 整个错过窗口合并为一次触发。 |
| `replay` | 把每个错过的触发都入队按序执行。**谨慎使用** —— 一个挂了一天的每分钟 cron 会试图跑 1440 次。 |

对于 DST 回拨日不明确的 wall time，错过枚举也遵循 take-first 策略，所以 `coalesce` 与 `replay` 不会在 overlap hour 里意外重复回填执行。

## 失败处理

- `failure_backoff`（默认 `expo(1m,2,1h)`）：指数 1m → 2m → 4m → ...，上限 1h。每分钟 cron 连续失败时，backoff 窗口内不会再发每分钟唤醒 —— 四小时调度器测试验证过。
- `pause_on_consecutive_failures`（默认 5）：连续 N 次终态失败后自动暂停。TUI 会把它显示为 disabled，附带 "paused after N failures" 标记。
- 首次成功后 `consecutive_failures` 复位为 0。

## 状态存在哪里

所有 cron 状态 —— 定义、catch-up watermark、run 历史 —— 都在 SQLite 数据库 `lad.sqlite` 中。没有单独的 cron 配置文件。

| 表 | 内容 |
|---|---|
| `crons` | 每条 cron 定义一行：schedule、project、backend、prompt、args、budget、`consecutive_failures`、`last_fired_at`、`next_fire_at`。 |
| `runs` | 每次触发一行：`scheduled_at`、`started_at`、`finished_at`、状态、退出码、cost、error。 |

每次 daemon 启动时，调度器内存堆都从 `crons` 重新播种。

旧的 `runs` 行按保留窗口（默认 90 天，每天本地 03:17 跑一次清理）裁剪。在删除前会先追加到按月的 `<state_dir>/runs/archive/<yyyymm>.jsonl.zst` 文件（zstd 压缩 JSONL），可以 grep 这些文件查历史，不必触及实时数据库。

## 安全注意事项

- **prompt 以明文存储。** 不加密、不按 secret 处理。不要在 prompt 里放凭据。
- **cron 触发的命令是直接 spawn 的** —— executor 不会用 `/bin/sh -c` 包一层。通过 `spawn_args` 做 shell 注入不会成功。
- **`cron_security` 里有 64 KiB prompt 上限（`MAX_PROMPT_BYTES`），但 v1 RPC 边界尚未启用**；daemon 今天会接受并存储任意大小的 prompt。仍请把 64 KiB 当作设计目标尊重 —— 一旦 `cron_security` 接上，超长 prompt 会在触不到 scheduler 之前被拒。

## 启用前先 dry-run

调 `crons.dry_run` 传 `count: N`（最多 20），查看在 cron 自身时区下的下 N 次触发时间。这是发现 `0 9 * * 7`（其实你想要 `0 9 * * 1`）等错误最便宜的方法。TUI 的 `R` 键今天驱动一个本地预览；live 源接入之后会 round-trip 到 daemon。

## 不启用也能跑一次

`crons.run_now` 绕过 schedule 立即触发一次，仍走和正常触发一样的 admission gate（配额依然生效）。新建 cron 后翻 enable 之前的健全性检查神器。TUI 的 `r` 键会在 live 源接入后驱动这件事；v1 请直接调 RPC。
