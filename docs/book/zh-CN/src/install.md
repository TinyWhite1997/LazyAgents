# 安装

LazyAgents 发布两个二进制 —— `lad`（daemon）与 `la`（TUI）。任意一种安装方式都会把两个都放到 `$PATH`。

> **注意 —— v1 平台范围。** 发布流水线产出 Linux x86_64/aarch64、macOS x86_64/aarch64、Windows x86_64 的二进制。当前仅 Linux 完成端到端验证。其它目标已经接入构建，但跨平台冒烟测试是 GA 的发布闸门，不是 Beta 的。macOS / Windows 上可能遇到的情况见[故障排查 → 平台说明](troubleshooting.md#平台说明)。

## 任选其一

### Linux / macOS —— `install.sh`

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/TinyWhite1997/LazyAgents/releases/latest/download/lazyagents-installer.sh | sh
```

安装器会把 `la` 与 `lad` 放进 `~/.cargo/bin`（或 `~/.local/bin`，取决于先找到哪个），并通过 sigstore 校验 artifact 的签名证明。

### Windows —— `install.ps1`

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/TinyWhite1997/LazyAgents/releases/latest/download/lazyagents-installer.ps1 | iex"
```

### Homebrew（状态：**等 tap 发布**）

每次发布都会生成 Homebrew formula，但 `TinyWhite1997/homebrew-tap` 仓库尚未发布。在那之前请用 `install.sh`。预期命令将是：

```sh
brew install TinyWhite1997/homebrew-tap/lazyagents
```

### Scoop

当前不提供。Windows 上请用 `install.ps1`。Scoop bucket 已作为后续 follow-up 项跟踪。

### 从源码构建

需要 Rust **1.75** 或更高（MSRV）。仓库 CI 固定使用 **1.96.0**；任何 ≥ 1.75 的工具链都能编译。

```sh
git clone https://github.com/TinyWhite1997/LazyAgents.git
cd LazyAgents
cargo install --path crates/la-daemon --locked
cargo install --path crates/la-tui    --locked
```

`la` 与 `lad` 会落到 `~/.cargo/bin`。

## 运行时前置条件

| 要求 | 原因 |
|---|---|
| **能用的终端模拟器**。任何支持 ANSI / VT 的 —— Kitty、Alacritty、WezTerm、iTerm2、Windows Terminal。 | TUI 使用 256 色与 OSC 序列。 |
| **`git` ≥ 2.20 在 `$PATH`**（仅在使用 worktree 评审时）。 | `lad` 通过 shell 调用 `git worktree`、`git apply`、`git diff-tree`。错误解析按英文 locale 模式匹配。 |
| **Windows 10 build 1809 及以上**（仅 Windows 用户）。 | Windows 上仅支持 ConPTY 作为 PTY 后端。 |

你**不**需要系统 SQLite —— LazyAgents 自带。你**不**需要 Node.js、Python 或任何容器运行时。

## 验证安装

```sh
la --version
lad --version
lad doctor
```

`lad doctor` 会打印解析后的 socket 与 state 目录，以及当前是否有 daemon 在监听。示例输出：

```text
socket path:    /run/user/1000/lazyagents/lad-1.sock
runtime dir:    /run/user/1000/lazyagents
state dir:      /home/alice/.local/share/lazyagents
server version: 0.1.1
status:         no daemon listening
```

最后一行在全新安装时是预期 —— `la` 会在首次启动时把 daemon 拉起来。

## 验证发布签名（可选）

LazyAgents 二进制通过 [GitHub Artifact Attestations](https://docs.github.com/en/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds)（sigstore 支持，免私钥）签名。如果你装了 `gh`：

```sh
gh attestation verify ./lazyagents-x86_64-unknown-linux-gnu.tar.xz \
  --repo TinyWhite1997/LazyAgents
```

## 各类数据的位置

| 项目 | 路径 | 覆盖变量 |
|---|---|---|
| 配置文件 | `$XDG_CONFIG_HOME/lazyagents/config.toml`（Linux） | `LAZYAGENTS_CONFIG_HOME` |
| SQLite 数据库 | `$XDG_DATA_HOME/lazyagents/lad.sqlite` | `LAZYAGENTS_DATA_DIR` |
| 会话脚本溢出文件（单条 > 8 MiB 时） | `$XDG_DATA_HOME/lazyagents/sessions/*.log` |（跟随 `LAZYAGENTS_DATA_DIR`） |
| 每会话 git worktree | `$XDG_DATA_HOME/lazyagents/worktrees/<project>/<sid>/` |（跟随 `LAZYAGENTS_DATA_DIR`） |
| Unix socket | `$XDG_RUNTIME_DIR/lazyagents/lad-1.sock` | `LAZYAGENTS_RUNTIME_DIR` |
| Windows 命名管道 | `\\.\pipe\lazyagents-lad-1` | — |

回退顺序：`$XDG_DATA_HOME` → `~/.local/share`；`$XDG_CONFIG_HOME` → `~/.config`；`$XDG_RUNTIME_DIR` → `/tmp/lazyagents-<UID>`。

## 更新

`la --check-update` 会查询 GitHub Releases，并告诉你是否有更新版。它不会自动安装 —— 用上面的安装器重新跑一次即可升级。

### Fork、内网镜像、离线网络

设置 `LAZYAGENTS_UPDATE_MANIFEST_URL` 把检查重定向到自己的 GitHub-兼容 Releases 端点。响应必须带 `tag_name`、`html_url`、`prerelease` 字段（你 fork 仓库的 GitHub Enterprise Server 镜像可直接复用）。这是唯一的开关 —— 没有第二条逃生通道，也没有可关的自动安装器。

```sh
export LAZYAGENTS_UPDATE_MANIFEST_URL=https://releases.internal.example.com/lazyagents/latest
la --check-update
```

如果指定的 URL 访问不通或返回体格式错误，`la --check-update` 会把简短原因写到 stderr 并以 `0` 退出 —— 这个检查刻意做成非致命的。

## 卸载

```sh
rm -f ~/.cargo/bin/la ~/.cargo/bin/lad   # 或 ~/.local/bin/{la,lad}
rm -rf ~/.local/share/lazyagents         # SQLite + 会话脚本 + worktree（你的数据！）
rm -rf ~/.config/lazyagents              # UI 偏好
```

LazyAgents 不会写入后端的数据目录 —— 卸载不会动 `~/.claude/projects/`、`~/.codex/sessions/`、`~/.local/share/opencode/`。那些属于对应工具自身。

下一步：[Quickstart →](quickstart.md)
