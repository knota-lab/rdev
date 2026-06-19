# rdev

rdev 是一个面向“本地编辑、远程构建/运行”工作流的开发辅助工具。

它把文件同步、远程命令执行、服务日志查看和进程管理收敛到一个命令行工具里，适合在 Windows 本地编辑代码、Linux 服务器上构建和运行服务的场景。

## Features

- 初始化项目级配置：`.rdev/config.toml`
- 单次全量同步：`rdev sync`
- 持续监听并增量同步：`rdev up`
- TUI 一体化控制台：`rdev up --tui`
- 远程命令执行：`rdev run "cargo test"` / `rdev run backend-lint`
- 远程 shell：`rdev ssh`
- SSH 认证检查：`rdev auth-check`
- 环境检查：`rdev doctor`

## Install

本地开发时可以直接构建 release 二进制：

```powershell
cargo build --release
```

如果要在正式开发项目里测试本仓库的开发构建，建议生成单独命名的开发二进制：

```powershell
.\scripts\dev-release.ps1
J:\cargo-target\release\rdev-dev.exe doctor
```

这样前台进程名是 `rdev-dev.exe`，它启动的后台 daemon 是 `rdev-dev-daemon.exe`。正式安装版仍然使用 `rdev.exe`，后台 daemon 是 `rdev-daemon.exe`，在任务管理器里更不容易误杀。

也可以安装到 Cargo bin 目录：

```powershell
cargo install --path . --force
```

如果只是替换本机正在使用的版本，可以直接复制 release 产物：

```powershell
Copy-Item target\release\rdev.exe "$env:USERPROFILE\.cargo\bin\rdev.exe" -Force
```

## Quick Start

在项目根目录初始化配置：

```powershell
rdev init --host root@example.com --path /root/my-project
```

检查本地和远程环境：

```powershell
rdev doctor
rdev auth-check
```

执行一次全量同步：

```powershell
rdev sync
```

启动持续同步：

```powershell
rdev up
```

启动 TUI 控制台：

```powershell
rdev up --tui
```

## TUI Commands

进入 `rdev up --tui` 后，底部输入区支持常用控制命令：

TUI 不展开 `alias`。需要复用常用服务命令时，使用 `new remote-session <name> -- <command>` 创建可停止、可恢复日志的 session；`alias` 只用于非 TUI 的 `rdev run` / `rdev exec` 远程命令执行。

```text
new session web -- pnpm dev
new remote-session api -- cd backend && cargo run
ps
logs web
tail web
stop web
restart web
sync
help
quit
quit!
```

常用快捷键：

```text
Ctrl+1..9   切换 process
Ctrl+Up     聚焦上一个 process
Ctrl+Down   聚焦下一个 process
Ctrl+C      复制日志选区；聚焦 sync 时取消当前同步
Esc         关闭 help 或清空输入
```

## Configuration

配置文件默认位于：

```text
.rdev/config.toml
```

示例：

```toml
version = 1

[remote]
host = "root@example.com"
port = 22
path = "/root/my-project"
identity_file = ""
passphrase_env = ""

[sync]
local_path = "."
watch_dirs = ["."]
exclude = [".git", "target", "node_modules", "data", ".rdev", ".idea", ".vscode", ".codegraph", "dist", "build"]
use_gitignore = true
debounce_ms = 300
direction = "push"
delete = true
delete_policy = "propagate"
full_sync_threshold = 32
backend = "auto"
rsync_mode = "auto"

[command]
default_shell = "bash"
remote_env = {}

[commands.backend-lint]
dir = "knota-fold"
command = "cargo clippy --all-features -- -D warnings"

[commands.l2-session]
dir = "knota-fold"
command = "cargo run -- task l2_process_session session_id:{session_id}"
```

`exclude` 支持用 `!` 写包含规则，例如排除所有 `data`，但保留任意路径下的 `src/data`：

```toml
exclude = ["data", "!src/data"]
```

`commands` 支持项目级远程服务器命令别名。别名可以携带执行目录，并支持 `key=value` 参数替换：

```powershell
rdev alias set backend-lint --dir knota-fold -- cargo clippy --all-features -- -D warnings
rdev alias set l2-session --dir knota-fold -- cargo run -- task l2_process_session session_id:{session_id}
rdev alias list
rdev run backend-lint
rdev exec l2-session -- session_id=26
```

alias 的边界：

- 只由 `rdev run <alias>` 和 `rdev exec <alias>` 展开。
- 只面向配置里的远程服务器，不是本地 shell alias。
- 不接入 TUI；TUI 里继续用 `new session` / `new remote-session` 管理常驻进程。
- `dir` 是相对 `remote.path` 的远程工作目录；显式 `--dir` 优先于 alias 自带的 `dir`。
- `alias set` 存在则更新，不存在则新增；`rdev alias delete <name>` 可删除别名。

## Exec Summary

`rdev exec --summary` 会把完整 stdout/stderr 写入本地 `.rdev/logs/`，终端只输出摘要：

```powershell
rdev exec --summary "cargo test"
rdev exec --summary l2-session -- session_id=26
```

摘要包含 exit code、执行目录、实际命令、完整日志路径、捕获行数/字节数、第一条 error/warn 线索和最后若干行日志。

## Backends

当前同步后端主要有两类：

- `rsync`：复用系统 rsync，适合已有 rsync 环境。
- `ssh` / `auto`：全量同步走 `ssh-tar`，增量同步走内置 SFTP。

Windows 下如果不想依赖 WSL rsync，可以优先使用默认的 `backend = "auto"`。

## Requirements

本机：

- Rust toolchain
- OpenSSH client
- 可用的 SSH key 或 ssh-agent

远端：

- SSH server
- `sh`
- `tar`
- 运行项目所需的语言工具链，例如 Rust、Node.js、pnpm 等

## Status

rdev 目前处于实用化早期阶段，主线能力是单向 push 同步和轻量 TUI process 管理。

暂不支持：

- 双向同步
- 完整交互式远程终端嵌入
- 多机器协同状态
- 远端 agent 服务
