# sgpt

`sgpt` 是面向命令行的 AI Shell 助手，调用 OpenAI-compatible Chat Completions API，并把当前 Shell 的 cwd、OS、架构和 Shell 信息作为上下文发送给 Provider。

它也可以把本机配置的 AI 能力投射到远端交互式 Shell：远端不安装 `sgpt` binary、不接收 API Key，也不需要直接访问 Provider。

当前版本：`0.6.0`。设计和实现说明见 [`docs/0.6.0.md`](docs/0.6.0.md)。

## 安装

需要 Rust 工具链：

```sh
cargo build --locked --release
cp target/release/sgpt ~/.local/bin/sgpt
```

也可以直接安装：

```sh
cargo install --path . --locked
```

正式支持 Linux/macOS 与 `/bin/sh`、bash、zsh、fish 交互环境；不支持原生 Windows Shell。BusyBox 环境为 best-effort。

## 配置

| 变量 | 必填 | 说明 |
| --- | --- | --- |
| `SGPT_BASE_URL` | 是 | Provider base URL，例如 `https://api.openai.com` 或 `https://host/v1`。不能填写完整 `/chat/completions` endpoint，不能包含 fragment；query 会保留。 |
| `SGPT_API_KEY` | 是 | API Key；不能包含控制字符或换行。 |
| `SGPT_MODEL` | 是 | 模型名称。 |
| `SGPT_SYSTEM_PROMPT` | 否 | 追加到内置 system prompt。 |
| `SGPT_PROXY` | 否 | HTTP/SOCKS proxy。 |
| `SGPT_TIMEOUT_SECONDS` | 否 | Provider timeout，`1..=600`，默认 `60`。 |
| `SGPT_PORT` | 否 | Origin Relay loopback 端口，`1024..=65535`；默认随机端口。 |
| `SGPT_MAX_PROJECTED_SESSIONS` | 否 | 每棵 Projection Tree 的 Projected Shell Session 上限，`1..=256`，默认 `64`。 |
| `SGPT_MAX_CONCURRENT_REQUESTS` | 否 | 每个 Origin Relay 的 Provider 并发上限，`1..=16`，默认 `4`。 |
| `SGPT_DEBUG` | 否 | 值为 `1` 时启用脱敏诊断。 |

```sh
export SGPT_BASE_URL=https://api.openai.com
export SGPT_API_KEY=sk-...
export SGPT_MODEL=gpt-4.1-mini
```

## 本地使用

```sh
sgpt "列出当前目录最大的 10 个文件"
git diff | sgpt "总结改动"
printf '日志内容' | sgpt
sgpt -c "继续上一轮"
printf '补充输入' | sgpt -c
sgpt -- "-n 在 grep 中是什么意思？"
sgpt --version
```

输入规则：

- 普通模式和 `continue` 都支持非空 stdin-only。
- 参数组成 instruction；非空 stdin 作为结构化 stdin，并在请求中渲染为 `Input:` 区块。
- 非 TTY stdin 为零字节时视为没有 stdin，不生成空 `Input:` 区块。
- 空白 stdin 是有效内容，不会 trim。
- prompt 以 `-` 开头时使用 `sgpt -- ...`；continue 中使用 `sgpt -c -- ...`。

每个 Local Shell Session 最多有一个 current Conversation。Conversation 只包含完整 Turn，即一次 user input 与一次成功 assistant response：

- 普通请求创建新 Conversation，并只在首个完整 Turn 持久化成功后替换 current。
- `continue` 只更新已有 current Conversation；不存在 current 时明确失败。
- Session lock 覆盖 Conversation 选择、Provider 调用、Turn commit、持久化和 current replacement。
- Provider 成功但 commit 或持久化失败时，不替换 current，也不输出回答。

本地 Conversation 位于私有运行时目录：优先 `$XDG_RUNTIME_DIR/sgpt`，否则 `${TMPDIR:-/tmp}/sgpt-<uid>`。目录和文件权限分别为 `0700`、`0600`。0.5.x version 1 JSONL 会在第一次成功写入时原子迁移为 version 2；损坏文件不会被当成空历史覆盖。

## SSH Projection

### 远端依赖

建立 tunnel 前，目标主机需要：

```text
sh curl jq base64 od tr mktemp mkfifo cat wc head id mkdir chmod rm uname
```

从已经投射的主机继续建立 nested Tunnel Hop 时，该中间主机还需要 `ssh`。

例如检查 `jq`：

```sh
ssh user@host 'command -v jq'
```

缺少依赖时 bootstrap 会输出 `sgpt remote dependency missing: <name>` 并退出。sgpt 不会把任意 SSH 非零退出误报为端口转发失败；真实 SSH、bootstrap 或远端依赖诊断会直接保留。

### 建立 Projection Tree

```sh
sgpt tunnel ssh user@host-a

# host-a 中：
sgpt "分析当前主机"
sgpt tunnel ssh user@host-b

# host-b 中仍可使用：
sgpt -c "继续分析"
```

每次根 `sgpt tunnel ssh` 创建一个 **Origin Relay** 和一棵独立 **Projection Tree**。每个 Tunnel Hop 创建独立 **Projected Shell Session**，但 nested hop 不会创建或联邦新的 relay。

Origin Relay 是所有 projected Conversations 的唯一权威来源：

- API Key、base URL、model 和 proxy 不发送到远端。
- 远端只持有临时 tree token、Session ID、当前输入和当前主机 ContextBlock。
- 远端不保存或传输 Conversation/history/current 文件。
- A、B 等不同 Projected Shell Sessions 的 Conversations 相互独立。
- Origin Relay 只监听 loopback。

Projected Shell Session 生命周期为 `Pending -> Active -> Closing -> Removed`：

- SSH preparation 创建 Pending；60 秒未 activate 会释放。
- bootstrap 成功后幂等 activate。
- 只有 Active Session 可以 ask 或创建 child Tunnel Hop。
- 退出时短超时 best-effort unregister；有 in-flight 请求时先进入 Closing，完成后删除。
- 删除 parent 会递归删除 descendants。
- 同一 Projected Shell Session 同时只允许一个请求；不同 Sessions 可以并发，并受全局并发上限约束。

`request_id` 是 Projected Shell Session 内的幂等键。transport failure 最多重试一次，并复用同一 request 文件和 ID：同 ID、同摘要返回已提交答案；同 ID、不同摘要返回 conflict。

升级 sgpt 或修改 bootstrap 后，已建立的 Projected Shell Session 不会热更新。需要退出整棵旧 Projection Tree，并从本机重新运行根 `sgpt tunnel ssh`。

### SSH policy

每一跳都在实际执行 SSH 的主机上先运行有界 `ssh -G`。因此用户和系统 `ssh_config`、`Host`、`Match`、`Include`、alias、变量展开与 OpenSSH 优先级仍由 OpenSSH 决定；sgpt 不扫描原始配置文件。

sgpt 只覆盖建立交互式 Projected Shell Session 所需的选项：

```text
RequestTTY=force
ExitOnForwardFailure=yes
RemoteCommand=none
SessionType=default
StdinNull=no
ForkAfterAuthentication=no
ClearAllForwardings=no
```

用户 argv 和 config 中的 `-L`、`-R`、`-D`、`LocalForward`、`RemoteForward`、`DynamicForward` 会保留。若规范化配置中的 TCP listener 使用 `SGPT_PORT`，preflight 会拒绝；target 使用相同端口不冲突，port `0` 和 Unix socket listener 允许。

以下模式不会建立受支持的交互式 Projected Shell Session，因此被拒绝，包括 attached 形式：

```text
-T -N -n -f -s -W -G -V -Q -O
```

不允许附加远端命令。`ssh -G` 只由 sgpt 内部 preflight 使用。

SSH 子进程退出码原样传播；被信号终止且没有状态码时返回 `255`。sgpt 自身的配置、preflight 或 relay 启动错误返回 `1`。Nested preflight 的 FIFO readers 和 `ssh -G` 在前台子 shell 内并发执行，不会向交互式 Bash 泄漏 `[1]`/`Done` job-control 通知。

## 资源上限

| 资源 | 上限与行为 |
| --- | --- |
| 当前 stdin | `512 KiB`；读取到 limit + 1 即失败。 |
| 近期 Conversation | 最多 10 个完整 Turns，软预算 `200 KiB` UTF-8。 |
| Input Anchors | 总软预算 `150 KiB`，最多 10 个；超限时保留最初一个与最近若干个，单个超限时 UTF-8 安全保留首尾。 |
| Provider Request JSON | 硬上限 `1 MiB`，按实际 JSON 序列化字节计算；必需内容自身超限时不调用 Provider。 |
| Provider HTTP response | 实际读取硬上限 `2 MiB`，成功和错误响应相同；支持 Content-Length 提前拒绝和 chunked 累计限制。 |
| assistant 文本 | `512 KiB` UTF-8。 |
| Conversation 逻辑内容 | 高水位 `2 MiB`，触发后压缩到约 `1.5 MiB`，始终保留最新完整 Turn。 |
| history JSONL | 物理安全上限 `16 MiB`；接近上限时按完整 Turn 压缩并原子重写。 |
| Relay Ask body | `4 MiB`。 |
| Relay Tunnel Prepare body | `512 KiB`。 |
| Relay Session control body | `64 KiB`。 |
| `ssh -G` stdout/stderr | 各 `256 KiB`，并发有界读取；超限时终止并回收子进程。 |

Provider Request 的可选内容优先级为：最新完整 Turn、Input Anchors、更多连续近期 Turns。Input Anchors 会对最终选中的近期 Turns 去重，任何路径都不会裁剪半个 Turn。

## 开发与验证

```sh
scripts/check.sh
```

等价于：

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

自动测试包括真实本地 fake Provider、临时文件系统、Axum Router、generated bootstrap `/bin/sh -n`、fake SSH executable harness、交互式 Bash pseudo-terminal、并发 lease 和 child process reap。

真实多主机 smoke 仍需手工执行，清单见 [`docs/0.6.0.md`](docs/0.6.0.md#手工-smoke-checklist)。

## 许可证

MIT，见 [LICENSE](LICENSE)。
