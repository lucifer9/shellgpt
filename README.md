# sgpt

`sgpt` 是一个面向命令行的 AI Shell 助手。它通过 OpenAI 兼容的 Chat Completions API 生成简洁的命令建议，并会自动携带当前目录、系统、架构、Shell 等上下文。

本项目重点解决一个常见痛点：**SSH 到远程主机后，也能像在本机一样使用 AI Shell 助手，而且不需要在远程主机安装任何 `sgpt` binary。**

## 项目特点

很多 Shell AI 工具只能在本机使用；如果想在远程机器上获得相同体验，通常需要在远端重复安装 binary、配置 API Key、同步模型配置，甚至处理远端无法访问 AI Provider 的网络问题。

`sgpt` 的设计不同：

- **远程主机零 binary 安装**：远端不需要安装 `sgpt`、Rust 程序或任何项目产物。
- **一次 `sgpt tunnel ssh`，远端立即可用**：登录远端后会自动进入一个交互式 Shell，并注入临时 `sgpt` 函数。
- **体验接近本机**：在远端直接运行 `sgpt "..."`、`sgpt -c "..."`，支持续聊、stdin、远端 cwd/OS/Shell 上下文。
- **支持多级 SSH 跳转后继续使用**：SSH 到远程后，可以在远端继续使用同样的 `sgpt tunnel ssh ...` 命令连接下一台主机；每一层都会继续注入 `sgpt`，保持一致的 AI 使用体验。
- **API Key 只留在本机**：远端不会获得 `SGPT_API_KEY`、`SGPT_BASE_URL`、`SGPT_MODEL` 或代理配置。
- **远端无需直连 AI Provider**：远端请求通过 SSH 反向端口转发回本机 relay，由本机调用模型服务。
- **会话临时化**：远端只持有本次 SSH 会话的临时 Token 和临时 Shell 注入，退出后清理。

也就是说，`sgpt` 不是“把 AI CLI 安装到每台服务器上”，而是“把本机 AI 能力安全地投射到当前 SSH 会话里”。

## 功能特性

- **OpenAI 兼容接口**：支持任何兼容 `/v1/chat/completions` 的服务。
- **上下文感知**：自动收集当前环境的 `cwd`、`uname`、`SHELL`、`/etc/os-release`、`sw_vers` 等信息；在远端使用时收集的是远端上下文。
- **管道输入**：可将 stdin 与提示词组合后发送给模型。
- **会话续聊**：使用 `-c` / `--continue` 继续当前终端或当前目录下的上一段对话。
- **SSH 隧道模式**：在远端交互式 Shell 中注入 `sgpt` 函数，远端请求经 SSH 反向端口转发回本机，由本机调用 AI 服务。
- **安全默认值**：本地 API Key 不会发送到远端；远端仅持有临时会话 Token。

## 安装

需要 Rust 工具链。

```sh
cargo build --release
```

将生成的二进制加入 `PATH`：

```sh
cp target/release/sgpt ~/.local/bin/sgpt
```

也可以在项目目录直接安装：

```sh
cargo install --path .
```

## 配置

运行前需要配置以下环境变量：

| 变量 | 必填 | 说明 |
| --- | --- | --- |
| `SGPT_BASE_URL` | 是 | OpenAI 兼容 API 的 Base URL，例如 `https://api.openai.com` 或 `https://api.example.com/v1`。不要填写完整的 `/chat/completions` 地址。 |
| `SGPT_API_KEY` | 是 | API Key。 |
| `SGPT_MODEL` | 是 | 模型名称。 |
| `SGPT_SYSTEM_PROMPT` | 否 | 追加到内置系统提示词后的自定义系统提示。 |
| `SGPT_PROXY` | 否 | reqwest 代理地址，支持 HTTP/SOCKS，例如 `socks5h://127.0.0.1:7890`。 |
| `SGPT_TIMEOUT_SECONDS` | 否 | 请求超时时间，范围 `1..=600`，默认 `60`。 |
| `SGPT_DEBUG` | 否 | 设为 `1` 时输出调试信息。 |

示例：

```sh
export SGPT_BASE_URL="https://api.openai.com"
export SGPT_API_KEY="sk-..."
export SGPT_MODEL="gpt-4.1-mini"
```

## 基本用法

### 提问

```sh
sgpt "列出当前目录下按大小排序的前 10 个文件"
```

如果提示词以 `-` 开头，用 `--` 分隔：

```sh
sgpt -- "-n 在 grep 里是什么意思？"
```

### 结合管道输入

```sh
git diff | sgpt "帮我总结这次改动"
```

当 stdin 不是 TTY 时：

- 只有 stdin：stdin 内容会直接作为提示词；
- 同时有参数和 stdin：参数会作为指令，stdin 内容会附加到 `Input:` 区块。

### 继续上一轮对话

```sh
sgpt "给一个 rsync 同步目录的安全命令"
sgpt -c "加上 dry-run"
sgpt --continue "再排除 node_modules"
```

会话历史保存在私有运行时目录中：

- 优先使用 `$XDG_RUNTIME_DIR/sgpt`；
- 否则使用 `${TMPDIR:-/tmp}/sgpt-<uid>`；
- 文件权限会限制为 `0700` / `0600`。

### 查看版本

```sh
sgpt --version
```

## SSH 隧道模式：远端零安装使用 AI

这是 `sgpt` 最核心的区别点。

`sgpt tunnel ssh` 会在本机启动一个 relay，通过 SSH 反向端口转发连接到远端，并在远端交互式 Shell 里注入一个临时 `sgpt` 函数。远端不需要安装任何 `sgpt` binary。

如果已经在远端会话里，也可以继续使用同样的命令 SSH 到下一台主机：

```sh
sgpt tunnel ssh user@host-a
# 进入 host-a 后继续：
sgpt tunnel ssh user@host-b
# 进入 host-b 后仍然可以：
sgpt "分析这台机器的系统信息"
```

每一层都会通过当前会话继续建立反向隧道和注入临时 `sgpt` 函数，因此可以在多级跳转后保持一致的 AI 使用体验。

```sh
sgpt tunnel ssh user@example.com
```

进入远端 Shell 后，像在本机一样使用：

```sh
sgpt "查看当前机器的磁盘使用情况"
sgpt -c "只看根分区"
uptime | sgpt "解释这台机器当前负载是否正常"
```

此时模型看到的是远端上下文，例如远端的 `pwd`、`uname`、`SHELL`、`/etc/os-release`。但真正的 AI 请求仍由本机完成。

工作方式：

1. 本机读取 `SGPT_BASE_URL`、`SGPT_API_KEY`、`SGPT_MODEL` 等 AI 配置。
2. 本机启动只监听 `127.0.0.1` 的 relay。
3. SSH 建立 `-R 127.0.0.1:<port>:127.0.0.1:<port>` 反向转发。
4. 远端 Shell 通过 bootstrap 脚本注入临时 `sgpt` 函数。
5. 远端执行 `sgpt "..."` 时，函数收集远端上下文和输入，通过反向隧道发回本机 relay。
6. 如果远端继续执行 `sgpt tunnel ssh ...`，会把同样的能力继续投射到下一台主机。
7. 本机 relay 调用 AI Provider，并把回答返回到当前最远端的终端。

远端不会获得本机的 `SGPT_API_KEY`、`SGPT_BASE_URL`、`SGPT_MODEL` 或 `SGPT_PROXY`。远端只持有本次会话的临时 `SGPT_SESSION_TOKEN`，用于访问本机 relay。

### SSH 参数

支持常见 SSH 参数，例如：

```sh
sgpt tunnel ssh -p 2222 -i ~/.ssh/id_ed25519 user@example.com
sgpt tunnel ssh -J jump.example.com user@example.com
```

限制：

- 必须是交互式 TTY，`-T` 不支持；
- 不支持附加远端命令，例如 `sgpt tunnel ssh host uptime`；
- 会强制启用 `ExitOnForwardFailure=yes`；
- 需要 SSH 服务端允许 TCP 转发。

可通过 `SGPT_PORT` 指定 relay 端口，范围 `1024..=65535`：

```sh
SGPT_PORT=18080 sgpt tunnel ssh user@example.com
```

### 远端依赖

远端不需要安装 `sgpt` binary，只需要常见系统工具：

- `sh`
- `curl`
- `jq`
- `base64`
- `od`
- `tr`
- `mktemp`
- `cat`
- `wc`

如果要从当前远端继续 `sgpt tunnel ssh` 到下一台主机，当前远端还需要安装 `ssh`。

## 限制

- stdin 最大 `512 KiB`。
- 发送给 AI Provider 的请求体最大 `1 MiB`；超出时会尝试丢弃较早历史。
- AI 响应体最大 `2 MiB`。
- 最终助手文本最大 `512 KiB`。
- 本地历史单文件最大 `2 MiB`。
- 单个 relay 同一时间只处理一个请求。

## 开发

```sh
cargo test
cargo run -- --version
```

常用源码入口：

- `src/main.rs`：命令分发。
- `src/cli.rs`：CLI 参数解析。
- `src/config.rs`：环境变量配置与校验。
- `src/ai/mod.rs`：OpenAI 兼容请求与响应解析。
- `src/history/mod.rs`：本机会话与历史存储。
- `src/tunnel/ssh.rs`：SSH 隧道启动与参数校验。
- `src/tunnel/bootstrap.rs`：远端 Shell 注入脚本。
- `src/relay/mod.rs`：本机 relay HTTP 服务。

## 许可证

MIT License。详见 [LICENSE](LICENSE)。
