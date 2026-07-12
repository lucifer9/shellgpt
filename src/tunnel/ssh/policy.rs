use anyhow::{bail, ensure};

pub const SSH_CONFIG_LIMIT: usize = 256 * 1024;
pub const SSH_DIAGNOSTIC_LIMIT: usize = 256 * 1024;

pub(in crate::tunnel) const CONTROLLED_OPTIONS: &[&str] = &[
    "RequestTTY=force",
    "ExitOnForwardFailure=yes",
    "RemoteCommand=none",
    "SessionType=default",
    "StdinNull=no",
    "ForkAfterAuthentication=no",
    "ClearAllForwardings=no",
];
const PROHIBITED_OPTIONS: &[&str] = &["-T", "-N", "-n", "-f", "-s", "-W", "-G", "-V", "-Q", "-O"];
const PROHIBITED_ATTACHED_PREFIXES: &[char] = &['W', 'Q', 'O'];
const VALUE_OPTIONS: &[char] = &[
    'B', 'b', 'c', 'D', 'E', 'e', 'F', 'I', 'i', 'J', 'L', 'l', 'm', 'O', 'o', 'P', 'p', 'Q', 'R',
    'S', 'w',
];
const ATTACHED_VALUE_OPTIONS: &[char] = &[
    'B', 'b', 'c', 'D', 'E', 'e', 'F', 'I', 'i', 'J', 'L', 'l', 'm', 'o', 'P', 'p', 'R', 'S', 'w',
];
const FLAG_OPTIONS: &[char] = &[
    '4', '6', 'A', 'a', 'C', 'K', 'k', 'M', 'q', 't', 'v', 'X', 'x', 'Y', 'y',
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ValidatedArgs {
    args: Vec<String>,
}

impl ValidatedArgs {
    pub(super) fn as_slice(&self) -> &[String] {
        &self.args
    }
}

pub(super) fn validate(args: &[String]) -> anyhow::Result<ValidatedArgs> {
    ensure!(!args.is_empty(), "sgpt tunnel ssh requires a destination.");
    let mut destination_seen = false;
    let mut after_double_dash = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if after_double_dash {
            if !destination_seen {
                destination_seen = true;
                i += 1;
                continue;
            }
            bail!("remote commands are not supported.");
        }
        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if destination_seen {
            bail!("remote commands are not supported.");
        }
        if prohibited_mode(arg) {
            bail!("SSH execution mode {arg} is incompatible with a Projected Shell Session.");
        }
        if option_takes_value(arg) {
            ensure!(
                args.get(i + 1).is_some(),
                "ssh option {arg} requires a value"
            );
            i += 2;
            continue;
        }
        if is_attached_value_option(arg) {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            validate_flag_option(arg)?;
            i += 1;
            continue;
        }
        destination_seen = true;
        i += 1;
    }
    ensure!(destination_seen, "sgpt tunnel ssh requires a destination.");
    Ok(ValidatedArgs {
        args: args.to_vec(),
    })
}

pub(super) fn validate_effective(
    _args: &ValidatedArgs,
    effective: &str,
    relay_port: Option<u16>,
) -> anyhow::Result<()> {
    ensure!(
        effective.len() <= SSH_CONFIG_LIMIT,
        "ssh -G output exceeded 256 KiB limit."
    );
    if let Some(port) = relay_port {
        for line in effective.lines() {
            let mut fields = line.split_ascii_whitespace();
            let Some(kind) = fields.next() else {
                continue;
            };
            if !matches!(
                kind.to_ascii_lowercase().as_str(),
                "localforward" | "remoteforward" | "dynamicforward"
            ) {
                continue;
            }
            let Some(listener) = fields.next() else {
                bail!("invalid {kind} in ssh -G output");
            };
            if listener_port(listener) == Some(port) {
                bail!("SSH forwarding listener conflicts with SGPT_PORT {port}: {line}");
            }
        }
    }
    Ok(())
}

pub(super) fn add_controlled_options(command: &mut tokio::process::Command) {
    for option in CONTROLLED_OPTIONS {
        command.args(["-o", option]);
    }
}

pub(in crate::tunnel) fn shell_controlled_arguments() -> String {
    CONTROLLED_OPTIONS
        .iter()
        .map(|option| format!("-o {}", super::shell_quote(option)))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(in crate::tunnel) fn shell_validation_function() -> String {
    let prohibited = PROHIBITED_OPTIONS.join("|");
    let prohibited_attached = PROHIBITED_ATTACHED_PREFIXES
        .iter()
        .map(|option| format!("-{option}?*"))
        .collect::<Vec<_>>()
        .join("|");
    let values = VALUE_OPTIONS
        .iter()
        .map(|option| format!("-{option}"))
        .collect::<Vec<_>>()
        .join("|");
    let attached = ATTACHED_VALUE_OPTIONS
        .iter()
        .map(|option| format!("-{option}?*"))
        .collect::<Vec<_>>()
        .join("|");
    let flags = FLAG_OPTIONS
        .iter()
        .map(char::to_string)
        .collect::<Vec<_>>()
        .join("|");

    format!(
        r#"_sgpt_validate_ssh_args() {{
  [ "$#" -gt 0 ] || {{ printf 'sgpt tunnel ssh requires a destination.\n' >&2; return 1; }}
  _sgpt_destination_seen=0
  _sgpt_after_double_dash=0
  while [ "$#" -gt 0 ]; do
    _sgpt_arg="$1"
    if [ "$_sgpt_after_double_dash" -eq 1 ]; then
      [ "$_sgpt_destination_seen" -eq 0 ] || {{ printf 'remote commands are not supported.\n' >&2; return 1; }}
      _sgpt_destination_seen=1
      shift
      continue
    fi
    if [ "$_sgpt_arg" = -- ]; then _sgpt_after_double_dash=1; shift; continue; fi
    [ "$_sgpt_destination_seen" -eq 0 ] || {{ printf 'remote commands are not supported.\n' >&2; return 1; }}
    case "$_sgpt_arg" in
      {prohibited}|{prohibited_attached})
        printf 'SSH execution mode %s is incompatible with a Projected Shell Session.\n' "$_sgpt_arg" >&2
        return 1 ;;
      {values})
        [ "$#" -gt 1 ] || {{ printf 'ssh option %s requires a value\n' "$_sgpt_arg" >&2; return 1; }}
        shift 2
        continue ;;
      {attached}) shift; continue ;;
      -*)
        _sgpt_flags="${{_sgpt_arg#-}}"
        [ -n "$_sgpt_flags" ] || {{ printf 'invalid SSH option %s\n' "$_sgpt_arg" >&2; return 1; }}
        while [ -n "$_sgpt_flags" ]; do
          _sgpt_flag="${{_sgpt_flags%"${{_sgpt_flags#?}}"}}"
          case "$_sgpt_flag" in {flags}) ;; *) printf 'unsupported SSH option %s\n' "$_sgpt_arg" >&2; return 1 ;; esac
          _sgpt_flags="${{_sgpt_flags#?}}"
        done
        shift
        continue ;;
      *) _sgpt_destination_seen=1; shift; continue ;;
    esac
  done
  [ "$_sgpt_destination_seen" -eq 1 ] || {{ printf 'sgpt tunnel ssh requires a destination.\n' >&2; return 1; }}
}}"#
    )
}

fn prohibited_mode(arg: &str) -> bool {
    PROHIBITED_OPTIONS.contains(&arg)
        || arg
            .as_bytes()
            .get(1)
            .is_some_and(|option| PROHIBITED_ATTACHED_PREFIXES.contains(&(*option as char)))
}

fn option_takes_value(arg: &str) -> bool {
    let bytes = arg.as_bytes();
    bytes.len() == 2 && bytes[0] == b'-' && VALUE_OPTIONS.contains(&(bytes[1] as char))
}

fn is_attached_value_option(arg: &str) -> bool {
    let bytes = arg.as_bytes();
    bytes.len() > 2 && bytes[0] == b'-' && ATTACHED_VALUE_OPTIONS.contains(&(bytes[1] as char))
}

fn validate_flag_option(arg: &str) -> anyhow::Result<()> {
    let bytes = arg.as_bytes();
    ensure!(
        bytes.len() >= 2 && bytes[0] == b'-' && bytes[1..].is_ascii(),
        "invalid SSH option {arg}"
    );
    ensure!(
        bytes[1..]
            .iter()
            .all(|flag| FLAG_OPTIONS.contains(&(*flag as char))),
        "unsupported SSH option {arg}"
    );
    Ok(())
}

fn listener_port(endpoint: &str) -> Option<u16> {
    if endpoint.starts_with('/') || endpoint.starts_with('~') {
        return None;
    }
    if let Ok(port) = endpoint.parse::<u16>() {
        return Some(port);
    }
    let port = if endpoint.starts_with('[') {
        endpoint.rsplit_once("]:").map(|(_, port)| port)
    } else {
        endpoint.rsplit_once(':').map(|(_, port)| port)
    }?;
    port.parse().ok()
}

#[cfg(test)]
pub(in crate::tunnel) mod fixtures {
    #[derive(Clone, Copy, Debug)]
    pub struct ArgsCase {
        pub name: &'static str,
        pub args: &'static [&'static str],
        pub accepted: bool,
    }

    pub const ARGS: &[ArgsCase] = &[
        ArgsCase {
            name: "destination",
            args: &["user@host"],
            accepted: true,
        },
        ArgsCase {
            name: "double dash destination",
            args: &["--", "user@host"],
            accepted: true,
        },
        ArgsCase {
            name: "attached port",
            args: &["-p2222", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "separated port",
            args: &["-p", "2222", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "attached local forward",
            args: &["-L1234:target:22", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "separated local forward",
            args: &["-L", "1234:target:22", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "attached remote forward",
            args: &["-R1234:target:22", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "separated remote forward",
            args: &["-R", "1234:target:22", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "attached dynamic forward",
            args: &["-D1080", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "separated dynamic forward",
            args: &["-D", "1080", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "attached jump",
            args: &["-Jjump", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "separated jump",
            args: &["-J", "jump", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "attached identity",
            args: &["-ikey", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "separated identity",
            args: &["-i", "key", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "attached config option",
            args: &["-oStrictHostKeyChecking=yes", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "separated config option",
            args: &["-o", "StrictHostKeyChecking=yes", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "combined flags",
            args: &["-vv", "host"],
            accepted: true,
        },
        ArgsCase {
            name: "shell metacharacters",
            args: &["-o", "ProxyCommand=printf '%s' '$HOME;$(id)'", "host name"],
            accepted: true,
        },
        ArgsCase {
            name: "missing destination",
            args: &[],
            accepted: false,
        },
        ArgsCase {
            name: "double dash missing destination",
            args: &["--"],
            accepted: false,
        },
        ArgsCase {
            name: "missing option value",
            args: &["-p"],
            accepted: false,
        },
        ArgsCase {
            name: "remote command",
            args: &["host", "uptime"],
            accepted: false,
        },
        ArgsCase {
            name: "invalid utf8-like option",
            args: &["-é", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "invalid cjk option",
            args: &["-中", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "single dash",
            args: &["-", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited tty",
            args: &["-T", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited no command",
            args: &["-N", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited stdin null",
            args: &["-n", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited fork",
            args: &["-f", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited subsystem",
            args: &["-s", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited stdio forward",
            args: &["-W", "target:22", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited attached stdio forward",
            args: &["-Wtarget:22", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited config query",
            args: &["-G", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited version",
            args: &["-V", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited query",
            args: &["-Q", "cipher", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited attached query",
            args: &["-Qcipher", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited control",
            args: &["-O", "check", "host"],
            accepted: false,
        },
        ArgsCase {
            name: "prohibited attached control",
            args: &["-Ocheck", "host"],
            accepted: false,
        },
    ];

    #[derive(Clone, Copy, Debug)]
    pub struct EffectiveCase {
        pub name: &'static str,
        pub effective: &'static str,
        pub port: u16,
        pub accepted: bool,
    }

    pub const EFFECTIVE: &[EffectiveCase] = &[
        EffectiveCase {
            name: "local listener conflict",
            effective: "localforward 127.0.0.1:18080 target:22\n",
            port: 18080,
            accepted: false,
        },
        EffectiveCase {
            name: "remote listener conflict",
            effective: "remoteforward [::1]:18080 target:22\n",
            port: 18080,
            accepted: false,
        },
        EffectiveCase {
            name: "dynamic listener conflict",
            effective: "dynamicforward 18080\n",
            port: 18080,
            accepted: false,
        },
        EffectiveCase {
            name: "target port is allowed",
            effective: "localforward 127.0.0.1:1234 target:18080\n",
            port: 18080,
            accepted: true,
        },
        EffectiveCase {
            name: "port zero is allowed",
            effective: "remoteforward 0 target:18080\n",
            port: 18080,
            accepted: true,
        },
        EffectiveCase {
            name: "unix socket is allowed",
            effective: "dynamicforward /tmp/sgpt.sock\n",
            port: 18080,
            accepted: true,
        },
        EffectiveCase {
            name: "tilde unix socket is allowed",
            effective: "localforward ~/.ssh/sgpt.sock target:22\n",
            port: 18080,
            accepted: true,
        },
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn shared_argument_fixtures_match_rust_policy() {
        for case in fixtures::ARGS {
            assert_eq!(
                validate(&strings(case.args)).is_ok(),
                case.accepted,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn shared_effective_config_fixtures_match_rust_policy() {
        let args = validate(&strings(&["host"])).unwrap();
        for case in fixtures::EFFECTIVE {
            assert_eq!(
                validate_effective(&args, case.effective, Some(case.port)).is_ok(),
                case.accepted,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn generated_shell_policy_is_deterministic() {
        assert_eq!(shell_validation_function(), shell_validation_function());
        assert_eq!(shell_controlled_arguments(), shell_controlled_arguments());
    }
}
