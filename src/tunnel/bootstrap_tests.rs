use super::bootstrap::stage1_script;
use super::ssh::policy::fixtures;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn bootstrap_is_valid_posix_shell_and_stateless() {
    let temp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(temp.path(), stage1_script()).unwrap();
    let output = Command::new("/bin/sh")
        .arg("-n")
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let script = stage1_script();
    assert!(!script.contains("_sgpt_history_json"));
    assert!(!script.contains("_sgpt_append_history"));
    assert!(!script.contains("/conversations"));
    assert!(!script.contains("SGPT_API_KEY"));
    assert!(script.contains("/v1/session/activate"));
    assert!(script.contains("/v1/session/unregister"));
    assert!(!script.contains("__SGPT_SSH_"));
    assert_eq!(script, stage1_script());
}

#[test]
fn bootstrap_bounds_stdin_before_request_construction() {
    let script = stage1_script();
    let head = script.find("head -c 524289").unwrap();
    let request = script.find("request.json").unwrap();
    assert!(head < request);
    assert!(script.contains("--rawfile stdin"));
    assert!(script.contains("_sgpt_attempt=1"));
}

#[test]
fn nested_tunnel_uses_shared_argument_policy_and_real_ssh_harness() {
    for case in fixtures::ARGS {
        let harness = NestedSshHarness::new("hostname example\n", "200");
        let output = harness.run(case.args);
        assert_eq!(
            output.status.success(),
            case.accepted,
            "{}: {}",
            case.name,
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = harness.calls();
        if case.accepted {
            assert!(calls.contains("PHASE=-G\n"), "{}", case.name);
            assert!(calls.contains("PHASE=final\n"), "{}", case.name);
            for option in super::ssh::policy::CONTROLLED_OPTIONS {
                assert!(calls.contains(option), "{} missing {option}", case.name);
            }
        } else {
            assert!(calls.is_empty(), "{} unexpectedly called ssh", case.name);
        }
    }
}

#[test]
fn nested_tunnel_effective_config_cases_reach_the_relay_adapter() {
    for case in fixtures::EFFECTIVE {
        let code = if case.accepted { "200" } else { "400" };
        let harness = NestedSshHarness::new(case.effective, code);
        let output = harness.run(&["host"]);
        assert_eq!(
            output.status.success(),
            case.accepted,
            "{}: {}",
            case.name,
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = harness.calls();
        assert!(calls.contains("PHASE=-G\n"), "{}", case.name);
        assert_eq!(
            calls.contains("PHASE=final\n"),
            case.accepted,
            "{}",
            case.name
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn nested_tunnel_hides_preflight_jobs_from_interactive_bash() {
    let harness = NestedSshHarness::new("hostname example\n", "200");
    let output = harness.run_interactive_bash(&["host"]);
    let terminal = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{terminal}");
    assert!(!terminal.contains("[1]"), "{terminal}");
    assert!(!terminal.contains("Done"), "{terminal}");
}

#[test]
fn bootstrap_restores_bash_login_fallback_chain() {
    let script = stage1_script();
    let bash = script
        .split("  bash)\n")
        .nth(1)
        .unwrap()
        .split("  zsh)")
        .next()
        .unwrap();
    let profile = bash.find(".bash_profile").unwrap();
    let login = bash.find(".bash_login").unwrap();
    let fallback = bash.find(".profile").unwrap();
    let bashrc = bash.find(".bashrc").unwrap();
    let common = bash.find("$_sgpt_common_rc").unwrap();
    assert!(bash.contains("elif [ -r \"$HOME/.bash_login\" ]"));
    assert!(bash.contains("elif [ -r \"$HOME/.profile\" ]"));
    assert!(profile < login && login < fallback && fallback < bashrc && bashrc < common);
}

#[test]
fn bootstrap_sources_real_fish_config_before_injecting_sgpt() {
    let script = stage1_script();
    let fish = script
        .split("  fish)\n")
        .nth(1)
        .unwrap()
        .split("  *)")
        .next()
        .unwrap();
    let user_config = fish
        .find("$HOME/.config/fish/config.fish")
        .expect("real HOME fish config is sourced");
    let injected = fish.find("function sgpt").unwrap();
    assert!(user_config < injected);
    assert!(fish.contains("_sgpt_cleanup_fish --on-event fish_exit"));
}

struct NestedSshHarness {
    _temp: tempfile::TempDir,
    script: std::path::PathBuf,
    session: std::path::PathBuf,
    calls: std::path::PathBuf,
}

impl NestedSshHarness {
    fn new(effective_config: &str, prepare_status: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        let session = temp.path().join("session");
        let calls = temp.path().join("ssh.calls");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&session).unwrap();

        let ssh = bin.join("ssh");
        std::fs::write(
            &ssh,
            r#"#!/bin/sh
if [ "${1:-}" = -G ]; then _sgpt_phase=-G; else _sgpt_phase=final; fi
printf 'PHASE=%s\n' "$_sgpt_phase" >>"$SGPT_TEST_SSH_CALLS"
for _sgpt_arg in "$@"; do printf 'ARG=%s\n' "$_sgpt_arg" >>"$SGPT_TEST_SSH_CALLS"; done
if [ "$_sgpt_phase" = -G ]; then printf '%s' "$SGPT_TEST_EFFECTIVE_CONFIG"; fi
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let curl = bin.join("curl");
        std::fs::write(
            &curl,
            r#"#!/bin/sh
_sgpt_output=
while [ "$#" -gt 0 ]; do
  if [ "$1" = -o ]; then shift; _sgpt_output="$1"; fi
  shift
done
if [ -n "$_sgpt_output" ]; then
  if [ "$SGPT_TEST_PREPARE_STATUS" = 200 ]; then
    printf '%s' '{"session_id":"fedcba9876543210","remote_command":"remote bootstrap"}' >"$_sgpt_output"
  else
    printf '%s' 'forwarding conflict' >"$_sgpt_output"
  fi
fi
printf '%s' "$SGPT_TEST_PREPARE_STATUS"
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();

        let common = common_rc_body();
        let script = temp.path().join("nested.sh");
        std::fs::write(&script, format!("{common}\n_sgpt_tunnel_ssh \"$@\"\n")).unwrap();

        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        // The path is stored in the script's sidecar environment file to keep run() small.
        std::fs::write(temp.path().join("path"), path).unwrap();
        std::fs::write(temp.path().join("effective"), effective_config).unwrap();
        std::fs::write(temp.path().join("status"), prepare_status).unwrap();
        Self {
            _temp: temp,
            script,
            session,
            calls,
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new("/bin/sh");
        command.arg(&self.script).args(args);
        self.configure(&mut command).output().unwrap()
    }

    #[cfg(target_os = "macos")]
    fn run_interactive_bash(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new("/usr/bin/script");
        command
            .args([
                "-q",
                "/dev/null",
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-i",
            ])
            .arg(&self.script)
            .args(args);
        self.configure(&mut command).output().unwrap()
    }

    fn configure<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        let root = self.script.parent().unwrap();
        command
            .env("PATH", std::fs::read_to_string(root.join("path")).unwrap())
            .env("SGPT_SESSION_DIR", &self.session)
            .env("SGPT_PORT", "18080")
            .env("SGPT_SESSION_TOKEN", "a".repeat(64))
            .env("SGPT_SESSION_ID", "0123456789abcdef")
            .env("SGPT_TEST_SSH_CALLS", &self.calls)
            .env(
                "SGPT_TEST_EFFECTIVE_CONFIG",
                std::fs::read_to_string(root.join("effective")).unwrap(),
            )
            .env(
                "SGPT_TEST_PREPARE_STATUS",
                std::fs::read_to_string(root.join("status")).unwrap(),
            )
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }
}

fn common_rc_body() -> String {
    stage1_script()
        .split("cat >\"$_sgpt_common_rc\" <<'SGPT_COMMON'\n")
        .nth(1)
        .and_then(|rest| rest.split("\nSGPT_COMMON").next())
        .expect("common rc heredoc exists")
        .to_string()
}

fn run_bootstrap_with_curl(
    http_code: &str,
    transport_status: i32,
    unregister_failure: bool,
) -> (std::process::Output, bool) {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let runtime = temp.path().join("runtime");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let shell_marker = temp.path().join("shell-started");
    let fake_curl = bin.join("curl");
    std::fs::write(
        &fake_curl,
        format!(
            "#!/bin/sh\ncase \"$*\" in *unregister*) [ {unregister_failure} = false ] || exit 7 ;; esac\nprintf '%s' {http_code}\nexit {transport_status}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fake_sh = bin.join("sh");
    std::fs::write(
        &fake_sh,
        format!(
            "#!/bin/sh\nprintf started > {}\n. \"$ENV\"\nexit 0\n",
            shell_marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = temp.path().join("stage1.sh");
    std::fs::write(&script, stage1_script()).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new("/bin/sh")
        .arg(&script)
        .env("PATH", path)
        .env("SHELL", &fake_sh)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("SGPT_PORT", "18080")
        .env("SGPT_SESSION_TOKEN", "a".repeat(64))
        .env("SGPT_SESSION_ID", "0123456789abcdef")
        .output()
        .unwrap();
    let shell_started = shell_marker.exists();
    let session_base = runtime.join("sgpt");
    if session_base.exists() {
        assert!(std::fs::read_dir(session_base).unwrap().next().is_none());
    }
    (output, shell_started)
}

#[test]
fn activation_accepts_only_2xx_before_starting_shell() {
    let (ok, started) = run_bootstrap_with_curl("200", 0, false);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(started);
    for code in ["404", "409", "500"] {
        let (output, started) = run_bootstrap_with_curl(code, 0, false);
        assert!(!output.status.success(), "HTTP {code} must fail activation");
        assert!(!started, "HTTP {code} must not start the shell");
    }
    let (output, started) = run_bootstrap_with_curl("000", 7, false);
    assert!(!output.status.success());
    assert!(!started);

    let (cleanup_failure, started) = run_bootstrap_with_curl("200", 0, true);
    assert!(cleanup_failure.status.success());
    assert!(started);
}

#[test]
fn bootstrap_executes_activate_and_unregister_without_remote_history() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    let runtime = temp.path().join("runtime");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = temp.path().join("curl.log");
    let fake_curl = bin.join("curl");
    std::fs::write(
        &fake_curl,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$SGPT_TEST_LOG\"\nprintf 200\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fake_sh = bin.join("sh");
    std::fs::write(&fake_sh, "#!/bin/sh\n. \"$ENV\"\n_sgpt_cleanup\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake_sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = temp.path().join("stage1.sh");
    std::fs::write(&script, stage1_script()).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new("/bin/sh")
        .arg(&script)
        .env("PATH", path)
        .env("SHELL", &fake_sh)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("SGPT_TEST_LOG", &log)
        .env("SGPT_PORT", "18080")
        .env("SGPT_SESSION_TOKEN", "a".repeat(64))
        .env("SGPT_SESSION_ID", "0123456789abcdef")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(calls.contains("/v1/session/activate"));
    assert!(calls.contains("/v1/session/unregister"));
    let runtime_entries = std::fs::read_dir(runtime.join("sgpt"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(runtime_entries.is_empty());
}
