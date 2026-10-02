use sha2::{Digest, Sha256};

pub fn fingerprint_secret(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let hash = hex::encode(hasher.finalize());
    let (prefix, suffix) = fingerprint_edges(value);
    format!(
        "<set:len={},prefix={},suffix={},sha256={}>",
        value.len(),
        prefix,
        suffix,
        &hash[..12]
    )
}

pub fn fingerprint_token(value: &str) -> String {
    let (prefix, suffix) = fingerprint_edges(value);
    format!(
        "<set:len={},prefix={},suffix={}>",
        value.len(),
        prefix,
        suffix
    )
}

fn fingerprint_edges(value: &str) -> (String, String) {
    // Four characters from each end would disclose an entire short credential.
    if value.chars().take(9).count() <= 8 {
        return ("<redacted>".into(), "<redacted>".into());
    }
    let prefix = value.chars().take(4).collect();
    let suffix_rev: String = value.chars().rev().take(4).collect();
    (prefix, suffix_rev.chars().rev().collect())
}

pub fn redact_provider_error(body: &str, secrets: &[&str]) -> String {
    let mut out = body.to_string();
    for secret in secrets.iter().copied().filter(|s| !s.is_empty()) {
        out = out.replace(secret, "<redacted-secret>");
    }
    redact_bearer_like(&out)
}

pub fn redact_proxy_password(proxy: &str) -> String {
    let Ok(mut url) = url::Url::parse(proxy) else {
        return proxy.to_string();
    };
    if url.password().is_some() {
        let _ = url.set_password(Some("<redacted-password>"));
    }
    url.to_string()
}

fn redact_bearer_like(input: &str) -> String {
    let mut words = input.split_whitespace().peekable();
    let mut out = String::new();
    while let Some(word) = words.next() {
        if !out.is_empty() {
            out.push(' ');
        }
        if word.eq_ignore_ascii_case("bearer") {
            out.push_str("Bearer");
            if words.peek().is_some() {
                let _ = words.next();
                out.push_str(" <redacted-secret>");
            }
        } else if word.len() >= 24
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            out.push_str("<redacted-secret>");
        } else {
            out.push_str(word);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_fingerprints_hide_edges_including_unicode_and_empty_values() {
        for (value, bytes) in [
            ("A", 1),
            ("", 0),
            ("abcd", 4),
            ("12345678", 8),
            ("中文🙂", 10),
            ("🙂🙂🙂🙂🙂🙂🙂🙂", 32),
        ] {
            assert_eq!(
                fingerprint_token(value),
                format!("<set:len={bytes},prefix=<redacted>,suffix=<redacted>>")
            );
            assert!(fingerprint_secret(value).starts_with(&format!(
                "<set:len={bytes},prefix=<redacted>,suffix=<redacted>,sha256="
            )));
        }
    }

    #[test]
    fn longer_fingerprints_preserve_character_boundaries_and_stable_identity() {
        assert_eq!(
            fingerprint_token("abcdefghij"),
            "<set:len=10,prefix=abcd,suffix=ghij>"
        );
        assert_eq!(
            fingerprint_secret("abcdefghij"),
            "<set:len=10,prefix=abcd,suffix=ghij,sha256=72399361da6a>"
        );
        assert_eq!(
            fingerprint_token("天地玄黄宇宙洪荒日月"),
            "<set:len=30,prefix=天地玄黄,suffix=洪荒日月>"
        );
    }

    #[test]
    fn redacts_known_secret_and_bearer_token() {
        let body = "bad sk-test Authorization: Bearer abcdefghijklmnopqrstuvwxyz";
        let redacted = redact_provider_error(body, &["sk-test"]);
        assert!(!redacted.contains("sk-test"));
        assert!(!redacted.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(redacted.contains("<redacted-secret>"));
    }

    #[test]
    fn redacts_proxy_password() {
        assert_eq!(
            redact_proxy_password("http://user:pass@127.0.0.1:7890"),
            "http://user:%3Credacted-password%3E@127.0.0.1:7890/"
        );
    }
}
