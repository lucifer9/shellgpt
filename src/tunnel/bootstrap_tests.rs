use super::bootstrap::stage1_script;

#[test]
fn stage1_template_has_no_local_ai_config_names() {
    let script = stage1_script();
    assert!(!script.contains("SGPT_API_KEY"));
    assert!(!script.contains("SGPT_BASE_URL"));
    assert!(!script.contains("SGPT_MODEL"));
    assert!(!script.contains("SGPT_PROXY"));
    assert!(script.contains("Authorization: Bearer $SGPT_SESSION_TOKEN"));
}

#[test]
fn stage1_template_checks_required_dependencies() {
    let script = stage1_script();
    for dep in ["curl", "jq", "base64", "od", "tr", "mktemp", "cat", "wc"] {
        assert!(script.contains(dep));
    }
}

#[test]
fn stage1_template_preserves_stdin_bytes_metadata() {
    let script = stage1_script();
    assert!(script.contains("_sgpt_stdin_bytes=0"));
    assert!(script.contains("_sgpt_stdin_bytes=\"$4\""));
    assert!(script.contains("--argjson stdin_bytes \"$_sgpt_stdin_bytes\""));
    assert!(script.contains("stdin_bytes:$stdin_bytes"));
    assert!(script.contains("stdin_bytes:.stdin_bytes"));
}

#[test]
fn common_rc_defines_cleanup_used_by_shell_exit_traps() {
    let common = common_rc_body();
    assert!(common.contains("_sgpt_cleanup()"));
    assert!(common.contains("rm -rf -- \"$SGPT_SESSION_DIR\""));
}

#[test]
fn zsh_startup_sources_login_locale_files_before_injection() {
    let branch = zsh_branch();
    assert!(branch.contains("exec zsh -l -i"));
    let zprofile = branch.find(".zprofile").unwrap();
    let zshrc = branch.find(".zshrc").unwrap();
    let zlogin = branch.find(".zlogin").unwrap();
    let common = branch.find("$_sgpt_common_rc").unwrap();
    assert!(zprofile < zshrc);
    assert!(zshrc < zlogin);
    assert!(zlogin < common);
}

#[test]
fn zsh_cleanup_uses_top_level_exit_trap_not_trapexit_function() {
    let branch = zsh_branch();
    assert!(branch.contains("trap '_sgpt_cleanup' EXIT"));
    assert!(!branch.contains("TRAPEXIT()"));
}

#[test]
fn bash_startup_loads_login_profile_chain_before_injection() {
    let branch = bash_branch();
    let etc_profile = branch.find("/etc/profile").unwrap();
    let bash_profile = branch.find(".bash_profile").unwrap();
    let bash_login = branch.find(".bash_login").unwrap();
    let profile = branch.find(".profile").unwrap();
    let bashrc = branch.find(".bashrc").unwrap();
    let common = branch.find("$_sgpt_common_rc").unwrap();
    assert!(etc_profile < bash_profile);
    assert!(bash_profile < bash_login);
    assert!(bash_login < profile);
    assert!(profile < bashrc);
    assert!(bashrc < common);
}

#[test]
fn fish_starts_as_login_interactive_shell() {
    let branch = fish_branch();
    assert!(branch.contains("exec fish -l -i"));
}

#[test]
fn nested_tunnel_embeds_bootstrap_env_in_remote_command() {
    let common = common_rc_body();
    assert!(common.contains("_sgpt_remote_bootstrap_cmd=\"SGPT_PORT=$(_sgpt_shell_quote"));
    assert!(common.contains("\"$_sgpt_remote_bootstrap_cmd\""));
    assert!(!common.contains("SGPT_STAGE1_B64=\"$_sgpt_stage1_b64\" \\\n    command ssh"));
}

#[test]
fn stage1_template_avoids_unlisted_remote_helpers() {
    let script = stage1_script();
    for dep in ["bass", "tail", "grep", "sed"] {
        assert!(!script.contains(dep));
    }
}

fn common_rc_body() -> &'static str {
    stage1_script()
        .split("cat >\"$_sgpt_common_rc\" <<'SGPT_COMMON'\n")
        .nth(1)
        .and_then(|rest| rest.split("\nSGPT_COMMON").next())
        .expect("common rc heredoc exists")
}

fn zsh_branch() -> &'static str {
    stage1_script()
        .split("  zsh)\n")
        .nth(1)
        .and_then(|rest| rest.split("  fish)").next())
        .expect("zsh branch exists")
}

fn bash_branch() -> &'static str {
    stage1_script()
        .split("  bash)\n")
        .nth(1)
        .and_then(|rest| rest.split("  zsh)").next())
        .expect("bash branch exists")
}

fn fish_branch() -> &'static str {
    stage1_script()
        .split("  fish)\n")
        .nth(1)
        .and_then(|rest| rest.split("  *)").next())
        .expect("fish branch exists")
}
