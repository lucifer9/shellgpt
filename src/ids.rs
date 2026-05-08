use rand::{TryRng, rngs::SysRng};

pub fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut data = vec![0_u8; bytes];
    SysRng.try_fill_bytes(&mut data)?;
    Ok(hex::encode(data))
}

pub fn session_token() -> anyhow::Result<String> {
    random_hex(32)
}

pub fn id128() -> anyhow::Result<String> {
    random_hex(16)
}

pub fn is_hex_id(value: &str) -> bool {
    (16..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn is_session_token(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_token_is_64_hex_chars() {
        let token = session_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn validates_id_lengths_and_hex_only() {
        assert!(is_hex_id("0123456789abcdef"));
        assert!(is_hex_id(&"a".repeat(64)));
        assert!(!is_hex_id("abc"));
        assert!(!is_hex_id(&"a".repeat(65)));
        assert!(!is_hex_id("0123456789abcdeg"));
    }
}
