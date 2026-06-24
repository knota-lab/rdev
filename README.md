# rdev

rdev 用于“本地写代码，远程构建/运行”的开发方式。它把文件同步、远程命令、服务启动和 TUI 日志面板放在一个工具里，主要面向 Windows 本地编辑、Linux 服务器运行项目的场景。

## 核心能力

- `rdev sync`：同步一次。
- `rdev up`：持续监听文件变化并同步。
- `rdev up --tui`：打开同步、session、daemon 状态集成界面。
- `rdev exec "cmd"`：通过本地 daemon 复用 SSH 连接执行远程命令。
- `rdev service start <name>`：启动远端长期服务，ready 后退出，服务留在后台。
- `rdev why-ignore <path>`：解释某个文件为什么没有同步。

## 安装

开发构建：

```powershell
cargo build --release
```

安装到 Cargo bin：

```powershell
cargo install --path . --force
```

如果只是替换当前机器上的版本，也可以直接复制 release 产物：

```powershell
Copy-Item target\release\rdev.exe "$env:USERPROFILE\.cargo\bin\rdev.exe" -Force
```

开发版建议用单独名字，避免和正式项目里运行的 `rdev.exe` 混淆：

```powershell
.\scripts\dev-release.ps1
J:\cargo-target\release\rdev-dev.exe doctor
```

## 快速开始

在项目根目录初始化配置：

```powershell
rdev init --host root@example.com --path /root/my-project
```

检查环境：

```powershell
rdev doctor
rdev auth-check
```

常用启动方式：

```powershell
rdev sync
rdev up
rdev up --tui
```

配置文件默认是：

```text
.rdev/config.toml
```

## 配置示例

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
exclude = [".git", "target", "node_modules", "data", ".rdev", "dist", "build"]
use_gitignore = true
debounce_ms = 300
delete = true
backend = "auto"

[command]
default_shell = "bash"
remote_env = {}

[commands.backend-lint]
dir = "backend"
command = "cargo clippy --all-features -- -D warnings"

[services.backend]
dir = "backend"
command = "cargo run"
ready_pattern = "listening on"
url = "http://10.124.124.0:5150"
```

`exclude` 支持 `!` 包含规则。下面表示排除任意路径下名为 `data` 的目录或文件，但保留任意路径下的 `src/data`：

```toml
exclude = ["data", "!src/data"]
```

## 远程命令

优先用 `exec`：

```powershell
rdev exec "pwd"
rdev exec "cargo test"
rdev exec backend-lint
rdev exec --summary "cargo test"
```

`exec` 会通过项目 daemon 复用 SSH 连接，适合 Codex、Claude Code 这类频繁执行命令的场景。`--summary` 会把完整日志写入 `.rdev/logs/`，终端只打印摘要。

`run` 是一次性 SSH 命令，不使用 daemon：

```powershell
rdev run "pwd"
```

它适合偶尔执行，或者排查 daemon 本身问题。

命令别名用 `alias set` 管理：

```powershell
rdev alias set backend-lint --dir backend -- cargo clippy --all-features -- -D warnings
rdev alias list
rdev alias delete backend-lint
```

alias 只会被 `rdev exec <alias>` 和 `rdev run <alias>` 展开，不接入 TUI。

## TUI

启动：

```powershell
rdev up --tui
```

常用命令：

```text
new session web -- pnpm dev
new remote-session api -- cd backend && cargo run
ps
logs web
tail web
stop web
enter web
restart web
sync
daemon status
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

TUI 里的 `new remote-session` 会启动外部 SSH 进程。SSH 断开后该 session 会退出，需要手动 `restart <name>` 或 `restore <name>`。

`enter [name|id]` 会给运行中的 session 发送一个回车；不带参数时发送给当前聚焦 session，适合在查看服务日志时插入分隔。

## 服务

`service` 适合给编程代理启动远端开发服务：本地命令等待 ready，ready 后退出 0，远端服务继续运行。

```powershell
rdev service set backend --dir backend --ready "listening on" --url http://10.124.124.0:5150 -- cargo run
rdev service start backend
rdev service status backend
rdev service logs backend
rdev service stop backend
```

默认 `start` / `wait` 只输出低频心跳和 ready summary，不回放完整日志。需要实时看启动日志时加 `--logs`：

```powershell
rdev service start --logs backend
rdev service wait --logs backend
```

ready 后会提示：

```text
logs_command=rdev service logs backend
status_command=rdev service status backend
stop_command=rdev service stop backend
```

ready 超时返回 `124`，但不会停止远端服务。可以继续用 `wait/status/logs/stop` 处理。

## 同步后端

- `backend = "auto"`：默认推荐。全量同步走 `ssh-tar`，增量同步走内置 SFTP。
- `backend = "rsync"`：复用系统 rsync，适合已有 rsync 环境。

TUI 的内置 SFTP 连接会缓存 SSH 连接。连接失效时会清掉旧连接、重连一次并重试当前同步；如果重连仍失败，下一次手动 `sync` 会重新建连接。

## 排障

```powershell
rdev doctor
rdev auth-check
rdev why-ignore path\to\file
rdev daemon status
```

`why-ignore` 会说明路径是 `ignored` 还是 `included`，以及命中了哪条 exclude/include 规则。

## 要求

本机：

- Rust toolchain
- OpenSSH client
- 可用的 SSH key 或 ssh-agent

远端：

- SSH server
- `sh`
- `tar`
- 项目需要的语言工具链，例如 Rust、Node.js、pnpm 等

## 当前边界

- 只做本地到远端的 push 同步。
- 不支持双向同步。
- 不做完整交互式远程终端嵌入。
- 暂不提供远端 agent 服务。
