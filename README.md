# sgpt

[English](README.md) | [中文](README_cn.md)

`sgpt` is an AI shell assistant for the command line. It calls an OpenAI-compatible Chat Completions API and sends the current shell's cwd, OS, architecture, and shell information to the provider as context.

It can also project locally configured AI capabilities into a remote interactive shell: the remote host does not install the `sgpt` binary, does not receive the API key, and does not need direct access to the provider.

Current version: `0.6.0`. Design and implementation notes: [`docs/0.6.0.md`](docs/0.6.0.md).

## Install

Requires a Rust toolchain:

```sh
cargo build --locked --release
cp target/release/sgpt ~/.local/bin/sgpt
```

Or install directly:

```sh
cargo install --path . --locked
```

Officially supported on Linux/macOS with interactive `/bin/sh`, bash, zsh, and fish environments. Native Windows shells are not supported. BusyBox environments are best-effort.

## Configuration

| Variable | Required | Description |
| --- | --- | --- |
| `SGPT_BASE_URL` | yes | Provider base URL, e.g. `https://api.openai.com` or `https://host/v1`. Do not set a full `/chat/completions` endpoint. Fragments are not allowed; query strings are preserved. |
| `SGPT_API_KEY` | yes | API key; must not contain control characters or newlines. |
| `SGPT_MODEL` | yes | Model name. |
| `SGPT_SYSTEM_PROMPT` | no | Appended to the built-in system prompt. |
| `SGPT_PROXY` | no | HTTP/SOCKS proxy. |
| `SGPT_TIMEOUT_SECONDS` | no | Provider timeout, `1..=600`, default `60`. |
| `SGPT_PORT` | no | Origin Relay loopback port, `1024..=65535`; defaults to a random port. |
| `SGPT_MAX_PROJECTED_SESSIONS` | no | Max Projected Shell Sessions per Projection Tree, `1..=256`, default `64`. |
| `SGPT_MAX_CONCURRENT_REQUESTS` | no | Max concurrent provider requests per Origin Relay, `1..=16`, default `4`. |
| `SGPT_DEBUG` | no | Set to `1` to enable redacted diagnostics. |

```sh
export SGPT_BASE_URL=https://api.openai.com
export SGPT_API_KEY=sk-...
export SGPT_MODEL=gpt-4.1-mini
```

## Local usage

```sh
sgpt "list the 10 largest files in the current directory"
git diff | sgpt "summarize the changes"
printf 'log content' | sgpt
sgpt -c "continue the previous turn"
printf 'additional input' | sgpt -c
sgpt -- "what does -n mean in grep?"
sgpt --version
```

Input rules:

- Both normal mode and `continue` support non-empty stdin-only input.
- Arguments form the instruction; non-empty stdin is structured stdin and rendered as an `Input:` block in the request.
- When non-TTY stdin is zero bytes, it is treated as no stdin and no empty `Input:` block is generated.
- Whitespace-only stdin is valid content and is not trimmed.
- If the prompt starts with `-`, use `sgpt -- ...`; in continue mode, use `sgpt -c -- ...`.

Each Local Shell Session has at most one current Conversation. A Conversation contains only complete Turns, i.e. one user input and one successful assistant response:

- A normal request creates a new Conversation and replaces the current one only after the first complete Turn is persisted successfully.
- `continue` only updates an existing current Conversation; it fails explicitly when none exists.
- The session lock covers Conversation selection, provider calls, Turn commit, persistence, and current replacement.
- If the provider succeeds but commit or persistence fails, the current Conversation is not replaced and no answer is printed.

Local Conversations live in a private runtime directory: `$XDG_RUNTIME_DIR/sgpt` if available, otherwise `${TMPDIR:-/tmp}/sgpt-<uid>`. Directory and file permissions are `0700` and `0600`. 0.5.x version 1 JSONL is atomically migrated to version 2 on the first successful write; corrupted files are never overwritten as empty history.

## SSH Projection

### Remote dependencies

Before establishing a tunnel, the target host needs:

```text
sh curl jq base64 od tr mktemp mkfifo cat wc head id mkdir chmod rm uname
```

When establishing a nested Tunnel Hop from an already projected host, that intermediate host also needs `ssh`.

For example, check for `jq`:

```sh
ssh user@host 'command -v jq'
```

If a dependency is missing, bootstrap prints `sgpt remote dependency missing: <name>` and exits. sgpt does not misreport arbitrary non-zero SSH exits as port-forward failures; real SSH, bootstrap, or remote dependency diagnostics are preserved as-is.

### Establishing a Projection Tree

```sh
sgpt tunnel ssh user@host-a

# on host-a:
sgpt "analyze the current host"
sgpt tunnel ssh user@host-b

# still usable on host-b:
sgpt -c "continue the analysis"
```

Each root `sgpt tunnel ssh` creates an **Origin Relay** and an independent **Projection Tree**. Each Tunnel Hop creates an independent **Projected Shell Session**, but nested hops do not create or federate a new relay.

The Origin Relay is the single source of truth for all projected Conversations:

- API key, base URL, model, and proxy are never sent to the remote host.
- The remote host only holds a temporary tree token, Session ID, current input, and the current host ContextBlock.
- The remote host never stores or transfers Conversation/history/current files.
- Conversations for different Projected Shell Sessions (e.g. A and B) are independent.
- The Origin Relay listens only on loopback.

Projected Shell Session lifecycle is `Pending -> Active -> Closing -> Removed`:

- SSH preparation creates Pending; unactivated sessions are released after 60 seconds.
- Bootstrap activates the session idempotently on success.
- Only Active Sessions can ask or create child Tunnel Hops.
- On exit, best-effort unregister runs with a short timeout; if requests are in flight, the session first enters Closing and is removed after they finish.
- Deleting a parent recursively deletes its descendants.
- Each Projected Shell Session allows only one request at a time; different Sessions may run concurrently, subject to the global concurrency limit.

`request_id` is the idempotency key within a Projected Shell Session. Transport failures retry at most once and reuse the same request file and ID: same ID and same summary returns the committed answer; same ID and different summary returns a conflict.

After upgrading sgpt or changing bootstrap, already established Projected Shell Sessions are not hot-reloaded. Exit the entire old Projection Tree and re-run root `sgpt tunnel ssh` from the local machine.

### SSH policy

Every hop first runs a bounded `ssh -G` on the host that will actually execute SSH. User and system `ssh_config`, `Host`, `Match`, `Include`, aliases, variable expansion, and OpenSSH precedence are therefore still decided by OpenSSH; sgpt does not scan raw config files.

sgpt only overrides the options required to establish an interactive Projected Shell Session:

```text
RequestTTY=force
ExitOnForwardFailure=yes
RemoteCommand=none
SessionType=default
StdinNull=no
ForkAfterAuthentication=no
ClearAllForwardings=no
```

`-L`, `-R`, `-D`, `LocalForward`, `RemoteForward`, and `DynamicForward` from user argv and config are preserved. If a normalized TCP listener uses `SGPT_PORT`, preflight rejects it; targets using the same port do not conflict. Port `0` and Unix socket listeners are allowed.

The following modes do not establish a supported interactive Projected Shell Session and are rejected, including attached forms:

```text
-T -N -n -f -s -W -G -V -Q -O
```

Attaching a remote command is not allowed. `ssh -G` is used only by sgpt's internal preflight.

SSH subprocess exit codes are propagated as-is; if terminated by a signal without a status code, `255` is returned. sgpt's own configuration, preflight, or relay startup errors return `1`. Nested preflight FIFO readers and `ssh -G` run concurrently inside a foreground subshell and do not leak `[1]`/`Done` job-control notifications into interactive Bash.

## Resource limits

| Resource | Limit and behavior |
| --- | --- |
| Current stdin | `512 KiB`; fails as soon as limit + 1 is read. |
| Recent Conversation | At most 10 complete Turns, soft budget `200 KiB` UTF-8. |
| Input Anchors | Soft budget `150 KiB` total, max 10; when exceeded, keep the first and the most recent ones; when a single item exceeds the limit, keep head and tail with UTF-8 safety. |
| Provider Request JSON | Hard limit `1 MiB`, measured by actual serialized JSON bytes; if required content alone exceeds the limit, the provider is not called. |
| Provider HTTP response | Hard read limit `2 MiB` for both success and error responses; Content-Length early rejection and chunked cumulative limits are supported. |
| Assistant text | `512 KiB` UTF-8. |
| Conversation logical content | High watermark `2 MiB`, then compacted to about `1.5 MiB`, always keeping the latest complete Turn. |
| history JSONL | Physical safety limit `16 MiB`; near the limit, compact by complete Turns and rewrite atomically. |
| Relay Ask body | `4 MiB`. |
| Relay Tunnel Prepare body | `512 KiB`. |
| Relay Session control body | `64 KiB`. |
| `ssh -G` stdout/stderr | `256 KiB` each, concurrent bounded reads; terminate and reap the child on overflow. |

Optional content priority for Provider Requests is: latest complete Turn, Input Anchors, then more contiguous recent Turns. Input Anchors are deduplicated against the finally selected recent Turns. No path trims a half Turn.

## Development and validation

```sh
scripts/check.sh
```

Equivalent to:

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

Automated tests cover a real local fake provider, temporary filesystems, Axum Router, generated bootstrap `/bin/sh -n`, fake SSH executable harness, interactive Bash pseudo-terminal, concurrent leases, and child process reaping.

Real multi-host smoke still requires manual execution; checklist: [`docs/0.6.0.md`](docs/0.6.0.md#手工-smoke-checklist).

## License

MIT, see [LICENSE](LICENSE).
