pub fn stage1_script() -> &'static str {
    STAGE1
}

const STAGE1: &str = r#"#!/bin/sh
set -u

_sgpt_die() { printf '%s\n' "$*" >&2; exit 1; }
_sgpt_have() { command -v "$1" >/dev/null 2>&1; }

for _sgpt_cmd in sh curl jq base64 od tr mktemp cat wc; do
  if ! _sgpt_have "$_sgpt_cmd"; then
    _sgpt_die "sgpt remote dependency missing: $_sgpt_cmd. Install standard system packages for sgpt v1 remote ask mode."
  fi
done

: "${SGPT_PORT:?missing SGPT_PORT}"
: "${SGPT_SESSION_TOKEN:?missing SGPT_SESSION_TOKEN}"
: "${SGPT_SESSION_ID:?missing SGPT_SESSION_ID}"
: "${SGPT_TIMEOUT_SECONDS:=60}"
: "${SGPT_VERSION:=0.1.0}"

case "$SGPT_PORT" in *[!0-9]*|'') _sgpt_die "invalid SGPT_PORT" ;; esac
case "$SGPT_TIMEOUT_SECONDS" in *[!0-9]*|'') _sgpt_die "invalid SGPT_TIMEOUT_SECONDS" ;; esac
case "$SGPT_SESSION_TOKEN" in *[!0123456789abcdefABCDEF]*|'') _sgpt_die "invalid SGPT_SESSION_TOKEN" ;; esac
if [ "${#SGPT_SESSION_TOKEN}" -ne 64 ]; then _sgpt_die "invalid SGPT_SESSION_TOKEN"; fi
case "$SGPT_SESSION_ID" in *[!0123456789abcdefABCDEF]*|'') _sgpt_die "invalid SGPT_SESSION_ID" ;; esac

umask 077
_sgpt_uid="$(id -u 2>/dev/null || printf '')"
if [ -n "${XDG_RUNTIME_DIR:-}" ] && [ -d "$XDG_RUNTIME_DIR" ]; then
  SGPT_RUNTIME_BASE="$XDG_RUNTIME_DIR/sgpt"
else
  [ -n "$_sgpt_uid" ] || _sgpt_die "failed to determine uid for sgpt runtime directory"
  SGPT_RUNTIME_BASE="${TMPDIR:-/tmp}/sgpt-$_sgpt_uid"
fi
mkdir -p "$SGPT_RUNTIME_BASE" || _sgpt_die "failed to create sgpt runtime base: $SGPT_RUNTIME_BASE"
chmod 700 "$SGPT_RUNTIME_BASE" 2>/dev/null || {
  [ "${SGPT_ACCEPT_INSECURE_RUNTIME_DIR:-}" = "1" ] || _sgpt_die "sgpt runtime base is not private: $SGPT_RUNTIME_BASE"
}
SGPT_SESSION_DIR="$(mktemp -d "$SGPT_RUNTIME_BASE/session-XXXXXXXXXX")" || _sgpt_die "failed to create sgpt session directory"
chmod 700 "$SGPT_SESSION_DIR" 2>/dev/null || true
mkdir -p "$SGPT_SESSION_DIR/conversations" "$SGPT_SESSION_DIR/rc" || _sgpt_die "failed to initialize sgpt session directory"
export SGPT_RUNTIME_BASE SGPT_SESSION_DIR SGPT_PORT SGPT_SESSION_TOKEN SGPT_SESSION_ID SGPT_TIMEOUT_SECONDS SGPT_VERSION

_sgpt_cleanup() {
  case "$SGPT_SESSION_DIR" in
    "$SGPT_RUNTIME_BASE"/session-*) rm -rf -- "$SGPT_SESSION_DIR" 2>/dev/null || {
      [ "${SGPT_DEBUG:-}" = "1" ] && printf '%s\n' "sgpt cleanup failed: $SGPT_SESSION_DIR" >&2
    } ;;
  esac
}

_sgpt_common_rc="$SGPT_SESSION_DIR/rc/sgpt-common.sh"
cat >"$_sgpt_common_rc" <<'SGPT_COMMON'
_sgpt_cleanup() {
  case "$SGPT_SESSION_DIR" in
    "$SGPT_RUNTIME_BASE"/session-*) rm -rf -- "$SGPT_SESSION_DIR" 2>/dev/null || {
      [ "${SGPT_DEBUG:-}" = "1" ] && printf '%s\n' "sgpt cleanup failed: $SGPT_SESSION_DIR" >&2
    } ;;
  esac
}

_sgpt_json_escape_file() {
  command jq -Rs . "$1"
}

_sgpt_random_id() {
  command od -An -N16 -tx1 /dev/urandom 2>/dev/null | command tr -d ' \n'
}

_sgpt_shell_quote() {
  printf '%s' "$1" | command jq -Rr @sh
}

_sgpt_now() {
  date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || printf ''
}

_sgpt_lock_acquire() {
  SGPT_LOCK_DIR="$SGPT_SESSION_DIR/lock"
  if ! mkdir "$SGPT_LOCK_DIR" 2>/dev/null; then
    printf 'sgpt session is locked at %s. If you are sure it is stale, remove it with: rmdir %s\n' "$SGPT_LOCK_DIR" "$SGPT_LOCK_DIR" >&2
    return 1
  fi
  chmod 700 "$SGPT_LOCK_DIR" 2>/dev/null || true
  {
    printf 'pid=%s\n' "$$"
    printf 'request_id=%s\n' "$1"
    printf 'created_at=%s\n' "$(_sgpt_now)"
    printf 'mode=%s\n' "$2"
  } >"$SGPT_LOCK_DIR/metadata"
  chmod 600 "$SGPT_LOCK_DIR/metadata" 2>/dev/null || true
}

_sgpt_lock_release() {
  [ -n "${SGPT_LOCK_DIR:-}" ] || return 0
  rm -f -- "$SGPT_LOCK_DIR/metadata" 2>/dev/null || true
  rmdir -- "$SGPT_LOCK_DIR" 2>/dev/null || true
  SGPT_LOCK_DIR=
}

_sgpt_build_prompt_file() {
  _sgpt_prompt_file="$1"
  shift
  _sgpt_args=
  _sgpt_continue=0
  _sgpt_stdin_bytes=0
  if [ "${1:-}" = "-c" ] || [ "${1:-}" = "--continue" ]; then
    _sgpt_continue=1
    shift
  fi
  if [ "${1:-}" = "--" ]; then shift; fi
  if [ "$#" -gt 0 ]; then
    _sgpt_args="$*"
  fi
  if [ -t 0 ]; then
    if [ -z "$_sgpt_args" ]; then
      printf 'prompt is required when stdin is a TTY.\n' >&2
      return 2
    fi
    printf '%s' "$_sgpt_args" >"$_sgpt_prompt_file"
  else
    _sgpt_stdin_file="$(mktemp "$SGPT_SESSION_DIR/stdin-XXXXXXXXXX")" || return 2
    command cat >"$_sgpt_stdin_file"
    _sgpt_size="$(wc -c <"$_sgpt_stdin_file" | tr -d ' ')"
    _sgpt_stdin_bytes="$_sgpt_size"
    if [ "$_sgpt_size" -gt 524288 ]; then
      rm -f -- "$_sgpt_stdin_file"
      printf 'stdin exceeded 512 KiB limit.\n' >&2
      return 2
    fi
    if [ -n "$_sgpt_args" ]; then
      {
        printf '%s\n\nInput:\n' "$_sgpt_args"
        command cat "$_sgpt_stdin_file"
      } >"$_sgpt_prompt_file"
    else
      command cat "$_sgpt_stdin_file" >"$_sgpt_prompt_file"
    fi
    rm -f -- "$_sgpt_stdin_file"
  fi
  return "$_sgpt_continue"
}

_sgpt_context_json() {
  _sgpt_ctx_file="$(mktemp "$SGPT_SESSION_DIR/context-XXXXXXXXXX")" || return 1
  {
    printf 'cwd=%s\n' "$(pwd 2>/dev/null || printf '')"
    printf 'uname_s=%s\n' "$(command uname -s 2>/dev/null || printf '')"
    printf 'uname_m=%s\n' "$(command uname -m 2>/dev/null || printf '')"
    printf 'shell=%s\n' "${SHELL:-}"
    printf 'os_release<<EOF\n'
    test -r /etc/os-release && command cat /etc/os-release 2>/dev/null || printf ''
    printf '\nEOF\n'
    printf 'sw_vers<<EOF\n'
    command sw_vers 2>/dev/null || printf ''
    printf '\nEOF\n'
  } >"$_sgpt_ctx_file"
  command jq -Rn '
    def trunc(n): if length > n then .[0:n-11] + "[truncated]" else . end;
    [inputs] as $lines |
    {
      cwd: (($lines[] | select(startswith("cwd=")) | .[4:]) // "" | trunc(4096)),
      uname_s: (($lines[] | select(startswith("uname_s=")) | .[8:]) // "" | trunc(128)),
      uname_m: (($lines[] | select(startswith("uname_m=")) | .[8:]) // "" | trunc(128)),
      shell: (($lines[] | select(startswith("shell=")) | .[6:]) // "" | trunc(1024)),
      os_release: "",
      sw_vers: ""
    }' <"$_sgpt_ctx_file"
  rm -f -- "$_sgpt_ctx_file"
}

_sgpt_history_json() {
  _sgpt_conversation_id="$1"
  _sgpt_history_file="$SGPT_SESSION_DIR/conversations/$_sgpt_conversation_id.jsonl"
  if [ ! -f "$_sgpt_history_file" ]; then printf '[]'; return 0; fi
  _sgpt_size="$(wc -c <"$_sgpt_history_file" | tr -d ' ')"
  if [ "$_sgpt_size" -gt 2097152 ]; then
    printf 'conversation history exceeded 2 MiB limit.\n' >&2
    return 1
  fi
  command jq -s 'map(select(.role == "user" or .role == "assistant") | {role,content,ts,stdin_bytes:.stdin_bytes})' "$_sgpt_history_file"
}

_sgpt_append_history() {
  _sgpt_conversation_id="$1"
  _sgpt_prompt_file="$2"
  _sgpt_answer_file="$3"
  _sgpt_stdin_bytes="$4"
  _sgpt_history_file="$SGPT_SESSION_DIR/conversations/$_sgpt_conversation_id.jsonl"
  _sgpt_user_line="$(command jq -nc --rawfile content "$_sgpt_prompt_file" --arg ts "$(_sgpt_now)" --argjson stdin_bytes "$_sgpt_stdin_bytes" '{role:"user",content:$content,ts:$ts} + if $stdin_bytes > 0 then {stdin_bytes:$stdin_bytes} else {} end')"
  _sgpt_assistant_line="$(command jq -nc --rawfile content "$_sgpt_answer_file" --arg ts "$(_sgpt_now)" '{role:"assistant",content:$content,ts:$ts}')"
  {
    printf '%s\n' "$_sgpt_user_line"
    printf '%s\n' "$_sgpt_assistant_line"
  } >>"$_sgpt_history_file" || return 1
  chmod 600 "$_sgpt_history_file" 2>/dev/null || true
}

_sgpt_ask() {
  if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    printf 'usage: sgpt [-c|--continue] <prompt...>\n       sgpt tunnel ssh [ssh args...]\n'
    return 0
  fi
  _sgpt_prompt_file="$(mktemp "$SGPT_SESSION_DIR/prompt-XXXXXXXXXX")" || return 1
  _sgpt_build_prompt_file "$_sgpt_prompt_file" "$@"
  _sgpt_status=$?
  if [ "$_sgpt_status" -eq 2 ]; then rm -f -- "$_sgpt_prompt_file"; return 1; fi
  _sgpt_continue="normal"
  [ "$_sgpt_status" -eq 1 ] && _sgpt_continue="continue"
  _sgpt_request_id="$(_sgpt_random_id)"
  [ -n "$_sgpt_request_id" ] || { printf 'failed to generate sgpt request id\n' >&2; rm -f -- "$_sgpt_prompt_file"; return 1; }
  _sgpt_lock_acquire "$_sgpt_request_id" "$_sgpt_continue" || { rm -f -- "$_sgpt_prompt_file"; return 1; }
  if [ "$_sgpt_continue" = "continue" ]; then
    if [ ! -f "$SGPT_SESSION_DIR/current" ]; then
      printf 'No previous sgpt conversation exists in this session.\nRun sgpt "..." first, then sgpt -c "..." to continue.\n' >&2
      _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file"; return 1
    fi
    _sgpt_conversation_id="$(cat "$SGPT_SESSION_DIR/current" | tr -d '\n')"
  else
    _sgpt_conversation_id="$(_sgpt_random_id)"
  fi
  case "$_sgpt_conversation_id" in *[!0123456789abcdefABCDEF]*|'') printf 'invalid conversation id\n' >&2; _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file"; return 1 ;; esac
  _sgpt_history="$(_sgpt_history_json "$_sgpt_conversation_id")" || { _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file"; return 1; }
  _sgpt_context="$(_sgpt_context_json)" || { _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file"; return 1; }
  _sgpt_request_file="$(mktemp "$SGPT_SESSION_DIR/request-XXXXXXXXXX")" || { _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file"; return 1; }
  command jq -nc \
    --arg request_id "$_sgpt_request_id" \
    --arg session_id "$SGPT_SESSION_ID" \
    --arg conversation_id "$_sgpt_conversation_id" \
    --rawfile prompt "$_sgpt_prompt_file" \
    --argjson history "$_sgpt_history" \
    --argjson context "$_sgpt_context" \
    '{request_id:$request_id,session_id:$session_id,conversation_id:$conversation_id,prompt:$prompt,history:$history,context:$context}' >"$_sgpt_request_file" || {
      _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file" "$_sgpt_request_file"; return 1
    }
  _sgpt_answer_file="$(mktemp "$SGPT_SESSION_DIR/answer-XXXXXXXXXX")" || { _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file" "$_sgpt_request_file"; return 1; }
  _sgpt_curl_time=$((SGPT_TIMEOUT_SECONDS + 5))
  [ "$_sgpt_curl_time" -gt 605 ] && _sgpt_curl_time=605
  _sgpt_http_code="$(curl -sS -o "$_sgpt_answer_file" -w '%{http_code}' \
    --connect-timeout 5 --max-time "$_sgpt_curl_time" \
    -H "Authorization: Bearer $SGPT_SESSION_TOKEN" \
    -H "Content-Type: application/json" \
    -H "Accept: text/plain" \
    --data-binary "@$_sgpt_request_file" \
    "http://127.0.0.1:$SGPT_PORT/v1/ask")"
  _sgpt_curl_status=$?
  if [ "$_sgpt_curl_status" -ne 0 ]; then
    printf 'curl failed to reach sgpt relay; the SSH remote forward may not be active.\n' >&2
    _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file" "$_sgpt_request_file" "$_sgpt_answer_file"; return 1
  fi
  case "$_sgpt_http_code" in
    2*) ;;
    *) command cat "$_sgpt_answer_file" >&2; printf '\n' >&2; _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file" "$_sgpt_request_file" "$_sgpt_answer_file"; return 1 ;;
  esac
  _sgpt_answer_size="$(wc -c <"$_sgpt_answer_file" | tr -d ' ')"
  if [ "$_sgpt_answer_size" -gt 524288 ]; then
    printf 'AI response exceeded 512 KiB limit.\n' >&2
    _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file" "$_sgpt_request_file" "$_sgpt_answer_file"; return 1
  fi
  _sgpt_append_history "$_sgpt_conversation_id" "$_sgpt_prompt_file" "$_sgpt_answer_file" "$_sgpt_stdin_bytes" || {
    _sgpt_lock_release; rm -f -- "$_sgpt_prompt_file" "$_sgpt_request_file" "$_sgpt_answer_file"; return 1
  }
  if [ "$_sgpt_continue" = "normal" ]; then
    printf '%s\n' "$_sgpt_conversation_id" >"$SGPT_SESSION_DIR/current.tmp" && mv "$SGPT_SESSION_DIR/current.tmp" "$SGPT_SESSION_DIR/current"
    chmod 600 "$SGPT_SESSION_DIR/current" 2>/dev/null || true
  fi
  _sgpt_lock_release
  command cat "$_sgpt_answer_file"
  if [ "$(command jq -Rs 'endswith("\n")' "$_sgpt_answer_file" 2>/dev/null)" != "true" ]; then
    printf '\n'
  fi
  rm -f -- "$_sgpt_prompt_file" "$_sgpt_request_file" "$_sgpt_answer_file"
}

_sgpt_tunnel_ssh() {
  if ! command -v ssh >/dev/null 2>&1; then
    printf 'sgpt remote dependency missing: ssh. Install ssh for nested tunnel mode.\n' >&2
    return 1
  fi
  if [ "$#" -eq 0 ]; then printf 'usage: sgpt tunnel ssh [ssh args...]\n' >&2; return 1; fi
  _sgpt_next_id="$(_sgpt_random_id)"
  [ -n "$_sgpt_next_id" ] || { printf 'failed to generate sgpt session id\n' >&2; return 1; }
  _sgpt_stage1_file="$(mktemp "$SGPT_SESSION_DIR/stage1-XXXXXXXXXX")" || return 1
  curl -sS --connect-timeout 5 --max-time 30 \
    -H "Authorization: Bearer $SGPT_SESSION_TOKEN" \
    "http://127.0.0.1:$SGPT_PORT/v1/bootstrap/sh" >"$_sgpt_stage1_file" || {
      printf 'failed to fetch sgpt bootstrap from local relay\n' >&2; rm -f -- "$_sgpt_stage1_file"; return 1
    }
  _sgpt_stage1_b64="$(base64 <"$_sgpt_stage1_file" | tr -d '\n')"
  rm -f -- "$_sgpt_stage1_file"
  _sgpt_remote_bootstrap_cmd="SGPT_PORT=$(_sgpt_shell_quote "$SGPT_PORT") SGPT_SESSION_TOKEN=$(_sgpt_shell_quote "$SGPT_SESSION_TOKEN") SGPT_SESSION_ID=$(_sgpt_shell_quote "$_sgpt_next_id") SGPT_TIMEOUT_SECONDS=$(_sgpt_shell_quote "$SGPT_TIMEOUT_SECONDS") SGPT_VERSION=$(_sgpt_shell_quote "$SGPT_VERSION") SGPT_STAGE1_B64=$(_sgpt_shell_quote "$_sgpt_stage1_b64") sh -c 'eval \"\$(printf %s \"\$SGPT_STAGE1_B64\" | base64 -d)\"'"
  command ssh -tt -o ExitOnForwardFailure=yes -R "127.0.0.1:$SGPT_PORT:127.0.0.1:$SGPT_PORT" "$@" "$_sgpt_remote_bootstrap_cmd"
}

sgpt() {
  if [ "${1:-}" = "tunnel" ]; then
    shift
    if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
      printf 'usage: sgpt tunnel ssh [ssh args...]\n'
      return 0
    fi
    if [ "${1:-}" = "ssh" ]; then shift; _sgpt_tunnel_ssh "$@"; return $?; fi
    printf 'usage: sgpt tunnel ssh [ssh args...]\n' >&2
    return 1
  fi
  if [ "${1:-}" = "--version" ]; then printf '%s\n' "$SGPT_VERSION"; return 0; fi
  _sgpt_ask "$@"
}
SGPT_COMMON

chmod 600 "$_sgpt_common_rc" 2>/dev/null || true

case "$(basename "${SHELL:-}" 2>/dev/null)" in
  fish) _sgpt_shell=fish ;;
  zsh) _sgpt_shell=zsh ;;
  bash) _sgpt_shell=bash ;;
  *) _sgpt_shell=sh ;;
esac

case "$_sgpt_shell" in
  bash)
    _sgpt_rc="$SGPT_SESSION_DIR/rc/bashrc"
    {
      printf '[ -r /etc/profile ] && . /etc/profile\n'
      printf 'if [ -r "$HOME/.bash_profile" ]; then\n'
      printf '  . "$HOME/.bash_profile"\n'
      printf 'elif [ -r "$HOME/.bash_login" ]; then\n'
      printf '  . "$HOME/.bash_login"\n'
      printf 'elif [ -r "$HOME/.profile" ]; then\n'
      printf '  . "$HOME/.profile"\n'
      printf 'fi\n'
      printf '[ -r "$HOME/.bashrc" ] && . "$HOME/.bashrc"\n'
      printf '. "%s"\n' "$_sgpt_common_rc"
      printf 'trap _sgpt_cleanup EXIT\n'
    } >"$_sgpt_rc"
    exec bash --rcfile "$_sgpt_rc" -i
    ;;
  zsh)
    _sgpt_zdotdir="$SGPT_SESSION_DIR/rc/zdotdir"
    mkdir -p "$_sgpt_zdotdir"
    {
      printf '[ -r "$HOME/.zshenv" ] && . "$HOME/.zshenv"\n'
    } >"$_sgpt_zdotdir/.zshenv"
    {
      printf '[ -r "$HOME/.zprofile" ] && . "$HOME/.zprofile"\n'
    } >"$_sgpt_zdotdir/.zprofile"
    {
      printf '[ -r "$HOME/.zshrc" ] && . "$HOME/.zshrc"\n'
    } >"$_sgpt_zdotdir/.zshrc"
    {
      printf '[ -r "$HOME/.zlogin" ] && . "$HOME/.zlogin"\n'
      printf '. "%s"\n' "$_sgpt_common_rc"
      printf "trap '_sgpt_cleanup' EXIT\n"
    } >"$_sgpt_zdotdir/.zlogin"
    ZDOTDIR="$_sgpt_zdotdir" exec zsh -l -i
    ;;
  fish)
    _sgpt_fishdir="$SGPT_SESSION_DIR/rc/fish"
    mkdir -p "$_sgpt_fishdir"
    cat >"$_sgpt_fishdir/config.fish" <<'EOF'
test -r "$HOME/.config/fish/config.fish"; and source "$HOME/.config/fish/config.fish"
function sgpt
  sh -c '. "$SGPT_COMMON_RC"; sgpt "$@"' sh $argv
end
function _sgpt_cleanup_fish --on-event fish_exit
  sh -c 'case "$SGPT_SESSION_DIR" in "$SGPT_RUNTIME_BASE"/session-*) rm -rf -- "$SGPT_SESSION_DIR" 2>/dev/null ;; esac'
end
EOF
    export SGPT_COMMON_RC="$_sgpt_common_rc"
    XDG_CONFIG_HOME="$SGPT_SESSION_DIR/rc" exec fish -l -i
    ;;
  *)
    _sgpt_rc="$SGPT_SESSION_DIR/rc/shrc"
    {
      printf '. "%s"\n' "$_sgpt_common_rc"
      printf 'trap _sgpt_cleanup EXIT\n'
    } >"$_sgpt_rc"
    ENV="$_sgpt_rc" exec sh -i
    ;;
esac
"#;
