use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ContextBlock {
    pub cwd: String,
    pub uname_s: String,
    pub uname_m: String,
    pub shell: String,
    pub os_release: String,
    pub sw_vers: String,
}

impl ContextBlock {
    pub fn truncated(mut self) -> Self {
        self.cwd = truncate_marker(&self.cwd, 4096);
        self.uname_s = truncate_marker(&self.uname_s, 128);
        self.uname_m = truncate_marker(&self.uname_m, 128);
        self.shell = truncate_marker(&self.shell, 1024);
        self.os_release = truncate_marker(&self.os_release, 4096);
        self.sw_vers = truncate_marker(&self.sw_vers, 2048);
        self
    }

    pub fn render(&self) -> String {
        let rendered = format!(
            "Current context:\ncwd: {}\nuname_s: {}\nuname_m: {}\nshell: {}\nos_release:\n{}\nsw_vers:\n{}",
            self.cwd, self.uname_s, self.uname_m, self.shell, self.os_release, self.sw_vers
        );
        truncate_marker(&rendered, 8192)
    }
}

pub async fn collect_local_context() -> ContextBlock {
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let shell = std::env::var("SHELL").unwrap_or_default();
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let uname_s = short_command("uname", &["-s"]).await;
    let uname_m = short_command("uname", &["-m"]).await;
    let sw_vers = if uname_s == "Darwin" {
        short_command("sw_vers", &[]).await
    } else {
        String::new()
    };
    ContextBlock {
        cwd,
        uname_s,
        uname_m,
        shell,
        os_release,
        sw_vers,
    }
    .truncated()
}

async fn short_command(program: &str, args: &[&str]) -> String {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return String::new(),
    };
    let mut stdout = child.stdout.take().expect("stdout piped");
    let output = async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, bytes))
    };
    match timeout(Duration::from_secs(1), output).await {
        Ok(Ok((status, bytes))) if status.success() => {
            String::from_utf8_lossy(&bytes).trim().to_string()
        }
        _ => {
            let _ = child.kill().await;
            String::new()
        }
    }
}

fn truncate_marker(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub("[truncated]".len());
    let mut out: String = value.chars().take(keep).collect();
    out.push_str("[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_context_fields_with_marker() {
        let context = ContextBlock {
            cwd: "a".repeat(5000),
            ..Default::default()
        }
        .truncated();
        assert_eq!(context.cwd.chars().count(), 4096);
        assert!(context.cwd.ends_with("[truncated]"));
    }
}
