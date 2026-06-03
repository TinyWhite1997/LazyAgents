# Worktree 评审

当你用 `worktree: true` 创建会话时，LazyAgents 会在拉起后端之前先执行 `git worktree add -b la/session-<short-sid> <base>`。agent 现在工作在它自己的分支与工作目录里，与主 checkout 完全隔离 —— 你还能拿到对它所有改动的内置 diff 评审。

## daemon 干了什么

在 `sessions.create { worktree: true }` 时：

1. 解析 base 分支：先试 `origin/HEAD`，回退到本地 `HEAD`。
2. 跑 `git worktree add -b la/session-<short-sid> <base-sha>`，原子地创建分支并 checkout。
3. 把 `worktree_path` 与 `worktree_branch` 写入 SQLite 的会话行。
4. 若 `<project>/.lazyagents/hooks/post-create.sh` 存在并可执行，运行它（60 s 限时，仅 advisory）。

任何一步在会话行写入之前失败，daemon 都会跑 `git worktree remove --force` 与 `git branch -D` 回滚。spawn 失败不会留下孤儿分支。

## worktree 落在哪儿

```
<state_dir>/worktrees/<project-slug>/<short-sid>/
```

- `state_dir`：`$LAZYAGENTS_DATA_DIR` 或 `$XDG_DATA_HOME/lazyagents` 或 `~/.local/share/lazyagents`。
- `project-slug`：`<仓库 basename>-<绝对路径 sha256 前 8 字节十六进制>`。哈希用来区分两个恰好同名的 checkout。
- `short-sid`：会话 UUID v7 的前 16 位十六进制。v7 的时间戳前缀让 `ls` 输出按时间排序。

示例：`/home/alice/code/myapp` 的会话落在 `/home/alice/.local/share/lazyagents/worktrees/myapp-a1b2c3d4/018e12345678abcd/`，分支 `la/session-018e12345678abcd`。

`la/` 分支命名空间是你的操作员逃生通道：`git branch --list 'la/session-*'` 列出仓库里所有 LazyAgents 拉起的分支。

## 要求

- `git` ≥ 2.20 在 `$PATH` —— daemon 对每个 worktree 操作都通过 shell 调它。没有 libgit2 后备。
- daemon 对每个 git 子进程强制设置 `LC_ALL=C`，锁定英文错误消息（错误分类器按英文模式匹配）。
- 每个 git 子进程都有 30 秒硬性墙钟超时。超时返回 `WorktreeProvision` 错误。

git 不存在会看到 `the git binary was not found on $PATH` —— 装 git 后重跑。

## 评审 diff

在 TUI 的会话视图（特别是 M2.5 加入的 worktree 面板）里有 7 个命令：

| 命令 | 效果 |
|---|---|
| `worktree.status` | 快照：分支、base、head、ahead/behind、每文件状态。 |
| `worktree.diff` | 单文件的 hunks，可选 staged 或 unstaged。 |
| `worktree.stage` | 把列出的 hunks 从工作树移到 index。 |
| `worktree.unstage` | 把列出的 hunks 从 index 移回工作树。 |
| `worktree.discard` | 把列出的 hunks 直接扔掉（需要显式确认 —— 见下文）。 |
| `worktree.commit` | `git commit -F -`，message 你自己定。 |
| `worktree.open_in_editor` | 在编辑器里打开 path:line:col。fire-and-forget。 |

diff 的每个 hunk 携带一个稳定的 `hunk_id`（path + range + body 字节的 SHA-256 指纹，取 16 位十六进制）。stage、unstage、discard 都按 hunk_id 操作。如果你读 diff 期间文件变了，过期的 id 会出现在响应的 `rejected` 数组里而不是静默错位 —— 重新拉 diff 再试。

### diff 大小

- 大于 **5 MiB** 的文件不会内联。diff 响应带 `truncated.hint: "open_in_editor"`、`hunks: []`。请改用编辑器打开。
- 二进制文件、子模块、不支持的文件类型同样被截断。
- `context_lines` 字段为向前兼容保留但**当前会被忽略** —— daemon 始终使用 `-U3`。

### discard 有闸门

`worktree.discard` 在你没传 `confirmed: true` 时拒绝做任何事。不知道这个字段的旧客户端默认 `false`，会拿到 `WORKTREE_DISCARD_UNCONFIRMED` —— fail-closed 的安全默认，防止意外丢失。

TUI 会弹一个确认弹窗；安全答案是 "否"。

### commit

`git commit -F -`，message 走 stdin。始终尊重你仓库的 `pre-commit` 与 `commit-msg` hook —— **永远不会**设 `--no-verify`。hook 拒绝 commit 时 daemon 会告诉你。

刻意不支持（今日）：`--amend`、`--signoff`、GPG 标志切换、自动 push。在 agent 正在跑的 session 上 `commit --amend` 是危险动作，我们明确不开放。

### 在编辑器打开

`worktree.open_in_editor` 按以下顺序解析编辑器：

1. params 里的 `editor_override`（非空）。
2. `$VISUAL`（非空）。
3. `$EDITOR`（非空）。
4. `code`（VS Code），仅当 `$PATH` 上找得到。
5. 否则返回 `WorktreeEditorUnavailable` 错误。

带嵌入标志的 `$EDITOR` 字符串也可以用：`EDITOR="code --wait"` 会按空白切分，标志前置。

按编辑器二进制名自动适配的参数语法：

| 编辑器二进制 | line 参数 | 支持 column |
|---|---|---|
| `code`、`code-insiders`、`cursor`、`vscodium`、`windsurf` | `--goto path:line:col` | 是 |
| `zed`、`zeditor` | `path:line:col` | 是 |
| `idea`、`pycharm`、`webstorm`、`rustrover`、`clion` | `--line <n> path` | 否 |
| `vim`、`nvim`、`vi` | `+<n> path` | 否 |
| `emacs`、`emacsclient` | `+<n> path` | 否 |
| 其它 | 仅 `path` | 否 |

编辑器以 fire-and-forget 方式拉起。daemon 一拿到子进程 PID 就返回；不会等编辑器退出。

## 通知

通过 `events.subscribe` 订阅 `worktree.changed`（任意变更）与 `worktree.commit_created`（commit 专用，附带 summary 与 files_changed 计数），可对变化作出响应。TUI 用它们刷新 diff 面板与弹 toast，不用轮询。

`kind: "external"` 的 `worktree.changed` 预留给 daemon 之外的改动（如 agent 或编辑器直接写 worktree）。daemon 当前不会触发它 —— 外部改动检测需要你自己的工具来做。

## archive vs delete（worktree 侧）

archive 会话时，daemon：

- 移除 worktree 目录（`git worktree remove --force`）。
- **保留分支**，当分支有超出 `base_branch` 的 commit 时（`CleanupMode::KeepBranchIfDirty`）。会话行的 `worktree_path` 清空；`worktree_branch` 保留，TUI 之后还可以提供 `git checkout`。

后台 sweep 会在保留期 TTL（默认 7 天）后用 `CleanupMode::Force` 强制清理已 archive 的 worktree 与分支。

硬性的 `sessions.delete` **不**触碰 worktree 目录或分支 —— 这是有意的，因为 "delete" 在 SQLite 里就是级联删除，我们不想让它静默销毁未合并的 commit。两者都想清掉的话，先 archive，让 sweeper 清理。

## git 报错时

`git worktree add` 失败的原始消息会通过 `CoreError::WorktreeProvision` 抛出来。常见的：

| 错误片段 | 含义 | 修复 |
|---|---|---|
| `'la/session-...' is already used by worktree at ...` | 上一次尝试留下了分支。 | `git worktree prune && git branch -D la/session-<sid>` 后重试。 |
| `fatal: not a git repository` | 项目根不是 git 仓库。 | `git init` 初始化，或换个项目。 |
| `fatal: invalid reference: origin/HEAD` | 没有 `origin/HEAD` symref。 | LazyAgents 会回退到本地 `HEAD`；正常情况下只是信息性提示，不应阻塞。如果阻塞了请提 issue。 |

手动恢复始终安全：`git worktree list` 显示所有（包括 LazyAgents 拉起的），`git worktree remove --force <path>` 撤销 daemon 的任何动作。
