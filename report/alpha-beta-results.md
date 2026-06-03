# Alpha / Beta 验证结果（M4.6）

**产品/功能**: LazyAgents TUI（Sessions / Crons / daemon attach 基础链路）
**关联 issue**: WEK-45 / M4.6
**标准**: WCAG 2.2 Level AA（终端 TUI 适用项）
**日期**: 2026-06-04
**审计角色**: AccessibilityAuditor
**范围调整**: 按 WEK-5 决策，v1 阶段只要求 Linux (`ubuntu-latest` 等价环境) 实测通过；macOS / Windows CI 与 ConPTY 实测降级为 GA 前 follow-up。

## 1. 结论

**当前结论：PARTIALLY CONFORMS / GA 不放行。**

Linux 自动化回归、TUI 渲染烟雾、键盘路径和 WCAG 对比度单测均通过；但本次本地验证环境无法完成真实外部 Alpha/Beta 用户招募，也无法运行真实屏幕阅读器（NVDA / JAWS / VoiceOver / Orca）人工会话。因此崩溃率、错误率、CSAT 与“至少 1 名键盘 + 屏幕阅读器用户完成 Story 1”仍属于 GA 阻断项，不能用绿色测试结果替代。

| 指标 | 目标 | 本次 Linux 结果 | 状态 |
|---|---:|---:|---|
| 自动化测试崩溃率 | < 1% | 0 / 583 = 0% | 通过 |
| 自动化测试错误率 | < 5% | 0 / 583 = 0% | 通过 |
| Alpha CSAT | >= 4 / 5 | 未采集真实用户评分 | 阻断 |
| Beta CSAT | >= 4 / 5 | 未开启 100 人 Beta | 阻断 |
| Onboarding 时长 | 需记录 | 未采集真实用户数据 | 阻断 |
| Story 1 键盘完成 | 需通过 | 代码级键盘路径覆盖；本地可达 | 部分通过 |
| Story 1 屏幕阅读器完成 | >= 1 名用户 | 未完成真实 AT 会话 | 阻断 |

## 2. 测试方法

### 自动化扫描 / 回归

- `cargo test --workspace --quiet`
  - 583 passed, 0 failed, 1 ignored
  - 覆盖 PTY、IPC、daemon、scheduler、storage、TUI render、Sessions / Crons 验收、对比度与键盘输入路由。
- `cargo fmt --check`
  - 通过。

### 键盘测试（代码级 + TestBackend）

已覆盖的关键路径：

- Sessions tab：`j/k`、方向键、`h/l`、`g/G`、`Enter`、`d`、`a`、`n`、`?`。
- Crons tab：`2` 切换、`n` 新建、`Tab` 字段切换、`Ctrl+S` 保存、`Space` 启停、`r` 立即触发、`R` dry-run、`d` 删除确认。
- Modals：`y` / `n` / `Esc` / `Enter` 路径覆盖，`Ctrl-C` 在 modal 内仍可退出。
- 长输出：10,000 行 transcript 可半页滚动并回到 follow 模式。
- CJK 宽字符：宽行渲染不 panic、不明显错位。

### 视觉 / 低视力测试

- Dark / Light 调色板的主要文本与状态色通过 WCAG 1.4.3 Contrast Minimum（AA，>= 4.5:1）单测。
- `Auto` 主题继承终端前景/背景，无法在应用内保证用户终端主题对比度；这是终端 TUI 的固有限制，报告为可接受风险但需文档提示。

### 屏幕阅读器测试

本次未完成真实 AT 测试。原因：当前执行环境没有可交互桌面会话与屏幕阅读器（NVDA / JAWS / VoiceOver / Orca）可用，不能诚实声明“1 名键盘 + 屏幕阅读器用户完成 Story 1”。以下仅为待执行脚本，不计入通过：

1. 在 Linux 桌面终端启用 Orca。
2. 启动 `la`，确认 tab、项目组、会话行、后端状态、底部 key hints 可由屏幕阅读器按终端文本顺序读出。
3. 仅用键盘完成 Story 1：启动 TUI、定位项目组、展开分组、移动到会话、按 `Enter` 打开、读取右侧会话内容。
4. 记录完成时间、卡点、误读文本、是否需要人工提示。

## 3. Alpha / Beta 结果

### Alpha（目标：团队 + 10 名终端重度用户）

**状态：未完成真实 cohort。**

本次仅完成 Linux 本地自动化与代码级键盘验证，不能替代 10 名外部/团队用户的 Alpha 反馈。建议 Alpha 表单最少采集：

| 字段 | 说明 |
|---|---|
| 环境 | OS、终端、shell、字体、是否 tmux/zellij |
| 后端 | Claude / Codex / OpenCode 安装与鉴权状态 |
| Story 1 完成时间 | 从启动 TUI 到打开一个会话 |
| 崩溃 | 是否 panic、退出码、crashes 文件 |
| 错误 | 灰态/错误提示是否可理解、是否能恢复 |
| CSAT | 1-5 分 |
| 辅助技术 | 键盘-only、屏幕阅读器、放大、高对比 |

### Beta（目标：公开招募 100 名）

**状态：未开启。**

Beta 不应在屏幕阅读器 Story 1 验证和 Alpha 指标缺失时启动。建议 Beta 放行前门槛：

- Alpha 至少 10 人完成 Story 1，崩溃率 < 1%，任务错误率 < 5%，CSAT >= 4/5。
- 至少 1 名屏幕阅读器用户完成 Story 1，无 Critical / Serious 阻断。
- Linux 发布包安装路径跑通；macOS / Windows 作为 follow-up 明确标注非 v1 验收。

## 4. WCAG 2.2 AA 审计概览

**总问题数：4**

- Critical: 1
- Serious: 1
- Moderate: 1
- Minor: 1

### Issue 1: 未完成真实屏幕阅读器用户验证

**WCAG Criterion**: 4.1.2 Name, Role, Value (Level A), 1.3.2 Meaningful Sequence (Level A)
**Severity**: Critical
**用户影响**: 屏幕阅读器用户可能无法确认当前焦点、项目分组、会话状态或右侧内容顺序，导致无法完成 Story 1。
**位置**: `la-tui` Sessions tab / terminal rendering
**证据**: 本次仅有 ratatui buffer 和键盘路由测试；没有真实 AT transcript。
**建议修复 / 验证**:

- 安排 1 名屏幕阅读器用户在 Linux 桌面终端执行 Story 1。
- 记录屏幕阅读器读出的项目组、会话行、后端状态、错误状态、key hints。
- 若读序不符合视觉顺序，提供 screen-reader mode：减少装饰符号、使用纯文本状态标签、确保焦点行文本自包含。

### Issue 2: Auto 主题无法保证用户终端对比度

**WCAG Criterion**: 1.4.3 Contrast Minimum (Level AA)
**Severity**: Serious
**用户影响**: 低视力用户在低对比终端主题下可能看不清正文、muted 文案或状态信息。
**位置**: `crates/la-tui/src/theme.rs` 的 `Theme::Auto`
**证据**: Dark / Light 固定 palette 有 AA 单测；Auto 明确继承终端默认颜色，不做对比度保证。
**建议修复 / 验证**:

- 首次启动或 `la doctor` 提示：若需要可读性保障，请切换 `T` 到 Dark / Light。
- 在帮助文档中明确 Auto 主题对比度取决于终端配置。
- Alpha 表单采集终端主题与可读性评分。

### Issue 3: 状态色仍需文本冗余确认

**WCAG Criterion**: 1.4.1 Use of Color (Level A)
**Severity**: Moderate
**用户影响**: 色盲、低视力或高对比模式用户不能只依赖颜色理解 backend / cron / daemon 状态。
**位置**: 状态栏、backend badges、cron validation。
**证据**: 现有测试确认 “not installed”“not logged in”等文本可见；但需要覆盖所有状态（running / idle / waiting / errored / enabled / disabled）。
**建议修复 / 验证**:

- 为所有状态 chip 保留纯文本标签，不只显示颜色或图标。
- 添加 render tests 覆盖 errored / running / disabled / enabled 的文本冗余。

### Issue 4: 真实放大/高对比/减少动态效果未手测

**WCAG Criterion**: 1.4.10 Reflow (Level AA), 2.3.3 Animation from Interactions (Level AAA advisory)
**Severity**: Minor
**用户影响**: 终端字号放大、系统高对比或 reduced motion 下，布局可能挤压、toast 可能过快消失。
**位置**: TUI 全局布局、detach notice、hint bar。
**证据**: 有窄宽度 render fallback 和 2 秒 toast 单测；没有真实终端 200% / 400% 放大手测。
**建议修复 / 验证**:

- 在 80x24、100x30、120x40 三档终端字号下跑 Story 1。
- 确认 `?` overlay、hint bar、status bar 不遮挡主要内容。
- reduced-motion 环境下避免非必要闪烁；toast 文本应可通过状态日志回看。

## 5. 做得好的地方

- 语义优先：Sessions / Crons 的键盘路径是应用主路径，不依赖鼠标。
- Key hints 与真实绑定共用同一注册表，降低“提示说能按但实际无效”的认知负担。
- Dark / Light palette 有 WCAG AA 对比度单测。
- Backend 不可用时显示灰态原因和 docs URL，而不是崩溃或静默失败。
- Cron 首次启用有确认和预算提示，降低误触发造成费用损失的风险。

## 6. Remediation Priority

### Immediate（GA 前必须完成）

1. 完成至少 1 名键盘 + 屏幕阅读器用户的 Linux Story 1 真实验证，并把 transcript / 完成时间 / 卡点补入本报告。
2. 完成 10 人 Alpha cohort，补齐崩溃率、错误率、CSAT、onboarding 时长。

### Short-term（Beta 前）

1. 给 Auto 主题增加可读性提示或默认引导到 Dark / Light。
2. 补 render tests，确认所有状态都有非颜色文本标签。

### Ongoing（Beta / GA）

1. 公开 Beta 前建立用户反馈表与 crash/error 汇总口径。
2. macOS / Windows + ConPTY 验证按 WEK-5 决策作为 follow-up，不阻塞 Linux v1，但阻塞三端 GA 宣称。

## 7. GA Gate

**不允许 GA。** 当前 Linux 自动化质量门通过，但用户指标与真实辅助技术验证缺失。只有在以下条件都满足后才能把本报告结论改为 “CONFORMS / GA allowed”：

- Alpha：>= 10 名终端重度用户完成核心路径，crash < 1%，error < 5%，CSAT >= 4/5。
- AT：>= 1 名键盘 + 屏幕阅读器用户独立完成 Story 1。
- Beta：100 人公开 Beta 指标继续满足 crash / error / CSAT 门槛。
