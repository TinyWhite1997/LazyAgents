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

TUI 切到 **Crons** tab，按 **`n`** 打开编辑器。`Tab` 在字段间循环：

| 字段 | 说明 |
|---|---|
| Name | 可读名字。 |
| Backend | `claude`、`codex`、`opencode`... |
| Spawn args | 传给后端 CLI 的额外参数。 |
| Cron expr | 5 字段或 6 字段。 |
| Tz | IANA 名。 |
| Prompt | 上限 64 KiB。**不要**在里面放凭据 —— 见 [安全注意事项](#安全注意事项)。 |
| Budget | 每日 USD 上限、每次最长运行时间、最大并发数等。 |

`Ctrl+S` 保存草稿。`Esc` 丢弃。

## Crons tab 键位

| 键 | 效果 |
|---|---|
| `j` / `k` / `↓` / `↑` | 移动光标 |
| `n` | 新建 cron 草稿 |
| `e` / `i` / `Enter` | 编辑高亮的 cron |
| `Space` | 切换 enabled/disabled（首次启用会显示确认弹窗 —— 见下文） |
| `r` | 立即触发此 cron 一次（`crons.run_now`） |
| `R` | dry-run —— 预览未来 5 次触发时间 |
| `d` | 删除（带确认弹窗） |
| `Ctrl+S` | 保存当前草稿 |
| `Esc` | 取消草稿 |

## 启用 cron 是两步确认

cron 在 save 后不会自动 enabled —— 你必须显式启用。第一次调用 `crons.set_enabled` 返回一个确认 token 与摘要（未来 5 次触发时间、每日估算 run 数、预算检查）。第二次调用必须在 5 分钟内带上该 token。这是有意设计：改动任何敏感字段（backend、args、prompt、schedule、timezone、runtime 限制、预算）会**自动 disable** cron 并使待用 token 失效，于是 "恶意编辑" 没法从 save 直接到 live 而绕过你。

TUI 里你只需按 `Space` 并确认；round-trip 是隐藏的。这层保护避免了某个被误启用的 cron 在夜里把你的 API 预算烧光。

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
- **prompt 在 IPC 边界限制 64 KiB**，触不到 scheduler 之前就会被截。

## 启用前先 dry-run

在 cron 行按 `R`（或调 `crons.dry_run` 传 `count: N`，最多 20），查看在 cron 自身时区下的下 N 次触发时间。这是发现 `0 9 * * 7`（其实你想要 `0 9 * * 1`）等错误最便宜的方法。

## 不启用也能跑一次

`crons.run_now`（键 `r`）绕过 schedule 立即触发一次，仍走和正常触发一样的 admission gate（配额依然生效）。新建 cron 后翻 enable 之前的健全性检查神器。
