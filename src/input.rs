use anyhow::{bail, ensure};
use std::io::{IsTerminal, Read};

pub const STDIN_LIMIT: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInput {
    pub prompt: String,
    pub stdin_bytes: usize,
}

pub fn compose_prompt<R: Read + IsTerminal>(
    prompt_args: Vec<String>,
    mut stdin: R,
) -> anyhow::Result<UserInput> {
    compose_prompt_with_is_tty(prompt_args, stdin.is_terminal(), &mut stdin)
}

pub fn compose_prompt_with_is_tty<R: Read>(
    prompt_args: Vec<String>,
    stdin_is_tty: bool,
    stdin: &mut R,
) -> anyhow::Result<UserInput> {
    let args_text = prompt_args.join(" ");
    let stdin_text = if stdin_is_tty {
        None
    } else {
        let mut bytes = Vec::new();
        let mut limited = stdin.by_ref().take((STDIN_LIMIT + 1) as u64);
        limited.read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= STDIN_LIMIT, "stdin exceeded 512 KiB limit.");
        let len = bytes.len();
        let text =
            String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("stdin must be valid UTF-8."))?;
        Some((text, len))
    };

    match (args_text.is_empty(), stdin_text) {
        (true, None) => bail!("prompt is required when stdin is a TTY."),
        (false, None) => Ok(UserInput {
            prompt: args_text,
            stdin_bytes: 0,
        }),
        (true, Some((stdin_text, stdin_bytes))) => Ok(UserInput {
            prompt: stdin_text,
            stdin_bytes,
        }),
        (false, Some((stdin_text, stdin_bytes))) => Ok(UserInput {
            prompt: format!("{args_text}\n\nInput:\n{stdin_text}"),
            stdin_bytes,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn composes_args_plus_stdin_payload() {
        let mut stdin = Cursor::new(b"hello\n".to_vec());
        let input =
            compose_prompt_with_is_tty(vec!["summarize".into(), "this".into()], false, &mut stdin)
                .unwrap();
        assert_eq!(input.prompt, "summarize this\n\nInput:\nhello\n");
        assert_eq!(input.stdin_bytes, 6);
    }

    #[test]
    fn stdin_alone_becomes_prompt() {
        let mut stdin = Cursor::new(b"hello".to_vec());
        let input = compose_prompt_with_is_tty(Vec::new(), false, &mut stdin).unwrap();
        assert_eq!(input.prompt, "hello");
    }

    #[test]
    fn rejects_stdin_over_limit() {
        let data = vec![b'a'; STDIN_LIMIT + 1];
        let mut stdin = Cursor::new(data);
        let err = compose_prompt_with_is_tty(vec!["x".into()], false, &mut stdin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("512 KiB"));
    }

    #[test]
    fn rejects_invalid_utf8_stdin() {
        let mut stdin = Cursor::new(vec![0xff]);
        let err = compose_prompt_with_is_tty(Vec::new(), false, &mut stdin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("UTF-8"));
    }
}
