pub fn stage1_script() -> String {
    STAGE1_TEMPLATE
        .replace(
            "__SGPT_SSH_VALIDATE_FUNCTION__",
            &super::ssh::policy::shell_validation_function(),
        )
        .replace(
            "__SGPT_SSH_CONTROLLED_OPTIONS__",
            &super::ssh::policy::shell_controlled_arguments(),
        )
}

const STAGE1_TEMPLATE: &str = r#"#!/bin/sh
set -u

_sgpt_die() { printf '%s\n' "$*" >&2; exit 1; }
_sgpt_have() { command -v "$1" >/dev/null 2>&1; }
for _sgpt_cmd in sh curl jq base64 od tr mktemp mkfifo cat wc head id mkdir chmod rm uname; do
  _sgpt_have "$_sgpt_cmd" || _sgpt_die "sgpt remote dependency missing: $_sgpt_cmd"
done
: "${SGPT_PORT:?missing SGPT_PORT}"
: "${SGPT_SESSION_TOKEN:?missing SGPT_SESSION_TOKEN}"
: "${SGPT_SESSION_ID:?missing SGPT_SESSION_ID}"
: "${SGPT_TIMEOUT_SECONDS:=60}"
: "${SGPT_VERSION:=0.6.0}"
case "$SGPT_PORT" in *[!0-9]*|'') _sgpt_die "invalid SGPT_PORT" ;; esac
case "$SGPT_SESSION_TOKEN" in *[!0123456789abcdefABCDEF]*|'') _sgpt_die "invalid SGPT_SESSION_TOKEN" ;; esac
[ "${#SGPT_SESSION_TOKEN}" -eq 64 ] || _sgpt_die "invalid SGPT_SESSION_TOKEN"
case "$SGPT_SESSION_ID" in *[!0123456789abcdefABCDEF]*|'') _sgpt_die "invalid SGPT_SESSION_ID" ;; esac

umask 077
_sgpt_uid="$(id -u 2>/dev/null || printf '')"
if [ -n "${XDG_RUNTIME_DIR:-}" ] && [ -d "$XDG_RUNTIME_DIR" ]; then SGPT_RUNTIME_BASE="$XDG_RUNTIME_DIR/sgpt"; else
  [ -n "$_sgpt_uid" ] || _sgpt_die "failed to determine uid"
  SGPT_RUNTIME_BASE="${TMPDIR:-/tmp}/sgpt-$_sgpt_uid"
fi
mkdir -p "$SGPT_RUNTIME_BASE" || _sgpt_die "failed to create runtime directory"
chmod 700 "$SGPT_RUNTIME_BASE" 2>/dev/null || { [ "${SGPT_ACCEPT_INSECURE_RUNTIME_DIR:-}" = 1 ] || _sgpt_die "runtime directory is not private"; }
SGPT_SESSION_DIR="$(mktemp -d "$SGPT_RUNTIME_BASE/session-XXXXXXXXXX")" || _sgpt_die "failed to create session directory"
mkdir -p "$SGPT_SESSION_DIR/rc" || _sgpt_die "failed to initialize session directory"
export SGPT_RUNTIME_BASE SGPT_SESSION_DIR SGPT_PORT SGPT_SESSION_TOKEN SGPT_SESSION_ID SGPT_TIMEOUT_SECONDS SGPT_VERSION

_sgpt_control() {
  _sgpt_path="$1"
  _sgpt_body="$SGPT_SESSION_DIR/control.json"
  command jq -nc --arg session_id "$SGPT_SESSION_ID" '{session_id:$session_id}' >"$_sgpt_body" || return 1
  _sgpt_code="$(curl -sS --connect-timeout 2 --max-time 5 -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $SGPT_SESSION_TOKEN" -H 'Content-Type: application/json' \
    --data-binary "@$_sgpt_body" "http://127.0.0.1:$SGPT_PORT$_sgpt_path")"
  _sgpt_status=$?
  [ "$_sgpt_status" -eq 0 ] || return "$_sgpt_status"
  case "$_sgpt_code" in 2??) return 0 ;; *) return 1 ;; esac
}
_sgpt_cleanup() {
  _sgpt_control /v1/session/unregister >/dev/null 2>&1 || true
  case "$SGPT_SESSION_DIR" in "$SGPT_RUNTIME_BASE"/session-*) rm -rf -- "$SGPT_SESSION_DIR" 2>/dev/null || true ;; esac
}
trap _sgpt_cleanup EXIT HUP INT TERM
_sgpt_control /v1/session/activate || _sgpt_die "failed to activate Projected Shell Session"

_sgpt_common_rc="$SGPT_SESSION_DIR/rc/sgpt-common.sh"
cat >"$_sgpt_common_rc" <<'SGPT_COMMON'
_sgpt_random_id() { command od -An -N16 -tx1 /dev/urandom 2>/dev/null | command tr -d ' \n'; }
_sgpt_cleanup() {
  _sgpt_body="$SGPT_SESSION_DIR/unregister.json"
  command jq -nc --arg session_id "$SGPT_SESSION_ID" '{session_id:$session_id}' >"$_sgpt_body" 2>/dev/null || return 0
  curl -sS --connect-timeout 1 --max-time 2 -o /dev/null -H "Authorization: Bearer $SGPT_SESSION_TOKEN" -H 'Content-Type: application/json' --data-binary "@$_sgpt_body" "http://127.0.0.1:$SGPT_PORT/v1/session/unregister" >/dev/null 2>&1 || true
  case "$SGPT_SESSION_DIR" in "$SGPT_RUNTIME_BASE"/session-*) rm -rf -- "$SGPT_SESSION_DIR" 2>/dev/null || true ;; esac
}
_sgpt_context_json() {
  _sgpt_os="$SGPT_SESSION_DIR/os-release"
  _sgpt_sw="$SGPT_SESSION_DIR/sw-vers"
  { test -r /etc/os-release && command head -c 4097 /etc/os-release; } >"$_sgpt_os" 2>/dev/null || :
  { command sw_vers 2>/dev/null | command head -c 2049; } >"$_sgpt_sw" || :
  command jq -nc --arg cwd "$(pwd 2>/dev/null || printf '')" --arg uname_s "$(uname -s 2>/dev/null || printf '')" --arg uname_m "$(uname -m 2>/dev/null || printf '')" --arg shell "${SHELL:-}" --rawfile os_release "$_sgpt_os" --rawfile sw_vers "$_sgpt_sw" '{cwd:$cwd,uname_s:$uname_s,uname_m:$uname_m,shell:$shell,os_release:$os_release,sw_vers:$sw_vers}'
}
_sgpt_ask() {
  _sgpt_mode=new
  if [ "${1:-}" = -h ] || [ "${1:-}" = --help ]; then printf 'usage: sgpt [-c|--continue] [prompt...]\n'; return 0; fi
  if [ "${1:-}" = -c ] || [ "${1:-}" = --continue ]; then _sgpt_mode=continue; shift; fi
  [ "${1:-}" = -- ] && shift
  _sgpt_instruction="$*"
  _sgpt_stdin="$SGPT_SESSION_DIR/stdin"
  : >"$_sgpt_stdin"
  if [ ! -t 0 ]; then command head -c 524289 >"$_sgpt_stdin"; fi
  _sgpt_size="$(wc -c <"$_sgpt_stdin" | tr -d ' ')"
  if [ "$_sgpt_size" -gt 524288 ]; then printf 'stdin exceeded 512 KiB limit.\n' >&2; return 1; fi
  if [ -z "$_sgpt_instruction" ] && [ "$_sgpt_size" -eq 0 ]; then printf 'prompt is required when stdin is a TTY.\n' >&2; return 1; fi
  _sgpt_context="$(_sgpt_context_json)" || return 1
  _sgpt_request_id="$(_sgpt_random_id)"
  [ -n "$_sgpt_request_id" ] || { printf 'failed to generate request id\n' >&2; return 1; }
  _sgpt_request="$SGPT_SESSION_DIR/request.json"
  command jq -nc --arg request_id "$_sgpt_request_id" --arg session_id "$SGPT_SESSION_ID" --arg mode "$_sgpt_mode" --arg instruction "$_sgpt_instruction" --rawfile stdin "$_sgpt_stdin" --argjson context "$_sgpt_context" '{request_id:$request_id,session_id:$session_id,mode:$mode,input:{instruction:$instruction,stdin:$stdin,timestamp:""},context:$context}' >"$_sgpt_request" || return 1
  _sgpt_answer="$SGPT_SESSION_DIR/answer"
  _sgpt_curl_time=$((SGPT_TIMEOUT_SECONDS + 5)); [ "$_sgpt_curl_time" -le 605 ] || _sgpt_curl_time=605
  _sgpt_attempt=0
  while :; do
    _sgpt_code="$(curl -sS -o "$_sgpt_answer" -w '%{http_code}' --connect-timeout 5 --max-time "$_sgpt_curl_time" --max-filesize 524288 -H "Authorization: Bearer $SGPT_SESSION_TOKEN" -H 'Content-Type: application/json' --data-binary "@$_sgpt_request" "http://127.0.0.1:$SGPT_PORT/v1/ask")"
    _sgpt_status=$?
    [ "$_sgpt_status" -eq 0 ] && break
    [ "$_sgpt_status" -ne 63 ] || { printf 'relay response exceeded 512 KiB limit.\n' >&2; return 1; }
    [ "$_sgpt_attempt" -eq 0 ] || { printf 'curl failed to reach sgpt relay.\n' >&2; return 1; }
    _sgpt_attempt=1
  done
  case "$_sgpt_code" in 2*) ;; *) command cat "$_sgpt_answer" >&2; printf '\n' >&2; return 1 ;; esac
  command cat "$_sgpt_answer"
  [ "$(command jq -Rs 'endswith("\n")' "$_sgpt_answer" 2>/dev/null)" = true ] || printf '\n'
}
__SGPT_SSH_VALIDATE_FUNCTION__
_sgpt_tunnel_ssh() {
  command -v ssh >/dev/null 2>&1 || { printf 'sgpt remote dependency missing: ssh.\n' >&2; return 1; }
  _sgpt_validate_ssh_args "$@" || return 1
  _sgpt_effective="$SGPT_SESSION_DIR/ssh-config"
  _sgpt_error="$SGPT_SESSION_DIR/ssh-error"
  _sgpt_stdout_fifo="$SGPT_SESSION_DIR/ssh-stdout-fifo"
  _sgpt_stderr_fifo="$SGPT_SESSION_DIR/ssh-stderr-fifo"
  command mkfifo "$_sgpt_stdout_fifo" "$_sgpt_stderr_fifo" || return 1
  (
    command head -c 262145 <"$_sgpt_stdout_fifo" >"$_sgpt_effective" & _sgpt_stdout_reader=$!
    command head -c 262145 <"$_sgpt_stderr_fifo" >"$_sgpt_error" & _sgpt_stderr_reader=$!
    command ssh -G __SGPT_SSH_CONTROLLED_OPTIONS__ "$@" >"$_sgpt_stdout_fifo" 2>"$_sgpt_stderr_fifo" &
    _sgpt_ssh_pid=$!
    wait "$_sgpt_ssh_pid"; _sgpt_ssh_status=$?
    wait "$_sgpt_stdout_reader" 2>/dev/null || true
    wait "$_sgpt_stderr_reader" 2>/dev/null || true
    command rm -f -- "$_sgpt_stdout_fifo" "$_sgpt_stderr_fifo"
    exit "$_sgpt_ssh_status"
  )
  _sgpt_ssh_status=$?
  _sgpt_size="$(wc -c <"$_sgpt_effective" | tr -d ' ')"
  [ "$_sgpt_size" -le 262144 ] || { printf 'ssh -G output exceeded 256 KiB limit.\n' >&2; return 1; }
  _sgpt_size="$(wc -c <"$_sgpt_error" | tr -d ' ')"
  [ "$_sgpt_size" -le 262144 ] || { printf 'ssh -G stderr exceeded 256 KiB limit.\n' >&2; return 1; }
  [ "$_sgpt_ssh_status" -eq 0 ] || { cat "$_sgpt_error" >&2; return 1; }
  _sgpt_argv="$(command jq -nc --args '$ARGS.positional' -- "$@")" || return 1
  _sgpt_prepare="$SGPT_SESSION_DIR/prepare.json"
  command jq -nc --arg parent_session_id "$SGPT_SESSION_ID" --argjson ssh_args "$_sgpt_argv" --rawfile effective_config "$_sgpt_effective" '{parent_session_id:$parent_session_id,ssh_args:$ssh_args,effective_config:$effective_config}' >"$_sgpt_prepare" || return 1
  _sgpt_response="$SGPT_SESSION_DIR/prepare-response.json"
  _sgpt_code="$(curl -sS -o "$_sgpt_response" -w '%{http_code}' --connect-timeout 5 --max-time 15 -H "Authorization: Bearer $SGPT_SESSION_TOKEN" -H 'Content-Type: application/json' --data-binary "@$_sgpt_prepare" "http://127.0.0.1:$SGPT_PORT/v1/tunnel/prepare")" || return 1
  case "$_sgpt_code" in 2*) ;; *) cat "$_sgpt_response" >&2; return 1 ;; esac
  _sgpt_child="$(jq -r .session_id "$_sgpt_response")"
  _sgpt_remote="$(jq -r .remote_command "$_sgpt_response")"
  command ssh __SGPT_SSH_CONTROLLED_OPTIONS__ -R "127.0.0.1:$SGPT_PORT:127.0.0.1:$SGPT_PORT" "$@" "$_sgpt_remote"
  _sgpt_status=$?
  if [ "$_sgpt_status" -ne 0 ]; then
    command jq -nc --arg session_id "$_sgpt_child" '{session_id:$session_id}' >"$SGPT_SESSION_DIR/cancel.json"
    curl -sS --connect-timeout 1 --max-time 2 -o /dev/null -H "Authorization: Bearer $SGPT_SESSION_TOKEN" -H 'Content-Type: application/json' --data-binary "@$SGPT_SESSION_DIR/cancel.json" "http://127.0.0.1:$SGPT_PORT/v1/session/cancel" >/dev/null 2>&1 || true
  fi
  return "$_sgpt_status"
}
sgpt() {
  if [ "${1:-}" = tunnel ]; then shift; [ "${1:-}" = ssh ] || { printf 'usage: sgpt tunnel ssh [ssh args...]\n' >&2; return 1; }; shift; _sgpt_tunnel_ssh "$@"; return $?; fi
  if [ "${1:-}" = --version ]; then printf '%s\n' "$SGPT_VERSION"; return 0; fi
  _sgpt_ask "$@"
}
SGPT_COMMON
chmod 600 "$_sgpt_common_rc" 2>/dev/null || true

_sgpt_shell_name="${SHELL:-}"
case "${_sgpt_shell_name##*/}" in fish) _sgpt_shell=fish ;; zsh) _sgpt_shell=zsh ;; bash) _sgpt_shell=bash ;; *) _sgpt_shell=sh ;; esac
case "$_sgpt_shell" in
  bash)
    _sgpt_rc="$SGPT_SESSION_DIR/rc/bashrc"
    { printf '[ -r /etc/profile ] && . /etc/profile\n'; printf 'if [ -r "$HOME/.bash_profile" ]; then . "$HOME/.bash_profile"\nelif [ -r "$HOME/.bash_login" ]; then . "$HOME/.bash_login"\nelif [ -r "$HOME/.profile" ]; then . "$HOME/.profile"\nfi\n'; printf '[ -r "$HOME/.bashrc" ] && . "$HOME/.bashrc"\n'; printf '. "%s"\n' "$_sgpt_common_rc"; printf 'trap _sgpt_cleanup EXIT\n'; } >"$_sgpt_rc"
    trap - EXIT HUP INT TERM; exec bash --rcfile "$_sgpt_rc" -i ;;
  zsh)
    _sgpt_zdotdir="$SGPT_SESSION_DIR/rc/zdotdir"; mkdir -p "$_sgpt_zdotdir"
    printf '[ -r "$HOME/.zshenv" ] && . "$HOME/.zshenv"\n' >"$_sgpt_zdotdir/.zshenv"
    printf '[ -r "$HOME/.zprofile" ] && . "$HOME/.zprofile"\n' >"$_sgpt_zdotdir/.zprofile"
    printf '[ -r "$HOME/.zshrc" ] && . "$HOME/.zshrc"\n' >"$_sgpt_zdotdir/.zshrc"
    { printf '[ -r "$HOME/.zlogin" ] && . "$HOME/.zlogin"\n'; printf '. "%s"\n' "$_sgpt_common_rc"; printf "trap '_sgpt_cleanup' EXIT\n"; } >"$_sgpt_zdotdir/.zlogin"
    trap - EXIT HUP INT TERM; ZDOTDIR="$_sgpt_zdotdir" exec zsh -l -i ;;
  fish)
    _sgpt_fishdir="$SGPT_SESSION_DIR/rc/fish"; mkdir -p "$_sgpt_fishdir"
    cat >"$_sgpt_fishdir/config.fish" <<'EOF'
test -r "$HOME/.config/fish/config.fish"; and source "$HOME/.config/fish/config.fish"
function sgpt
  sh -c '. "$SGPT_COMMON_RC"; sgpt "$@"' sh $argv
end
function _sgpt_cleanup_fish --on-event fish_exit
  sh -c '. "$SGPT_COMMON_RC"; _sgpt_cleanup'
end
EOF
    export SGPT_COMMON_RC="$_sgpt_common_rc"; trap - EXIT HUP INT TERM; XDG_CONFIG_HOME="$SGPT_SESSION_DIR/rc" exec fish -l -i ;;
  *)
    _sgpt_rc="$SGPT_SESSION_DIR/rc/shrc"; { printf '. "%s"\n' "$_sgpt_common_rc"; printf 'trap _sgpt_cleanup EXIT\n'; } >"$_sgpt_rc"
    trap - EXIT HUP INT TERM; ENV="$_sgpt_rc" exec sh -i ;;
esac
"#;
