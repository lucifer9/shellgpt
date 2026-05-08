use sha2::{Digest, Sha256};

pub fn fingerprint_secret(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let hash = hex::encode(hasher.finalize());
    let prefix: String = value.chars().take(4).collect();
    let suffix_rev: String = value.chars().rev().take(4).collect();
    let suffix: String = suffix_rev.chars().rev().collect();
    format!(
        "<set:len={},prefix={},suffix={},sha256={}>",
        value.len(),
        prefix,
        suffix,
        &hash[..12]
    )
}

pub fn fingerprint_token(value: &str) -> String {
    let prefix: String = value.chars().take(4).collect();
    let suffix_rev: String = value.chars().rev().take(4).collect();
    let suffix: String = suffix_rev.chars().rev().collect();
    format!(
        "<set:len={},prefix={},suffix={}>",
        value.len(),
        prefix,
        suffix
    )
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
