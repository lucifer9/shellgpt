use crate::conversation::UserInput;
use anyhow::{bail, ensure};
use std::io::{IsTerminal, Read};

pub const STDIN_LIMIT: usize = 512 * 1024;

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
        if len == 0 { None } else { Some(text) }
    };

    match (args_text.is_empty(), stdin_text) {
        (true, None) => bail!("prompt is required when stdin is a TTY."),
        (false, None) => Ok(UserInput::new(args_text, "")),
        (true, Some(stdin_text)) => Ok(UserInput::new("", stdin_text)),
        (false, Some(stdin_text)) => Ok(UserInput::new(args_text, stdin_text)),
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
        assert_eq!(input.instruction, "summarize this");
        assert_eq!(input.stdin, "hello\n");
        assert_eq!(input.rendered(), "summarize this\n\nInput:\nhello\n");
    }

    #[test]
    fn stdin_alone_becomes_prompt() {
        let mut stdin = Cursor::new(b"hello".to_vec());
        let input = compose_prompt_with_is_tty(Vec::new(), false, &mut stdin).unwrap();
        assert_eq!(input.instruction, "");
        assert_eq!(input.stdin, "hello");
    }

    #[test]
    fn zero_byte_pipe_is_no_stdin_and_does_not_create_input_block() {
        let mut stdin = Cursor::new(Vec::new());
        let input = compose_prompt_with_is_tty(vec!["hello".into()], false, &mut stdin).unwrap();
        assert_eq!(input.rendered(), "hello");
        let err = compose_prompt_with_is_tty(Vec::new(), false, &mut Cursor::new(Vec::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("prompt is required"));
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
