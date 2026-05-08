use anyhow::{bail, ensure};
use reqwest::Proxy;
use std::time::Duration;
use url::Url;

#[derive(Clone, Debug)]
pub struct AiConfig {
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub system_prompt: Option<String>,
    pub proxy: Option<String>,
    pub timeout: Duration,
    pub debug: bool,
}

impl AiConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let base_url = env_required("SGPT_BASE_URL")?;
        let api_key = env_required("SGPT_API_KEY")?;
        let model = env_required("SGPT_MODEL")?;
        validate_api_key(&api_key)?;
        Ok(Self {
            endpoint: normalize_base_url(&base_url)?,
            api_key,
            model,
            system_prompt: std::env::var("SGPT_SYSTEM_PROMPT").ok(),
            proxy: std::env::var("SGPT_PROXY").ok().filter(|s| !s.is_empty()),
            timeout: parse_timeout(std::env::var("SGPT_TIMEOUT_SECONDS").ok())?,
            debug: crate::debug::enabled(),
        })
    }

    pub fn reqwest_client(&self) -> anyhow::Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder().timeout(self.timeout);
        if let Some(proxy) = &self.proxy {
            builder = builder.proxy(Proxy::all(proxy)?);
        }
        Ok(builder.build()?)
    }
}

fn env_required(name: &str) -> anyhow::Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => bail!("{name} is not set."),
    }
}

pub fn normalize_base_url(input: &str) -> anyhow::Result<String> {
    let trimmed = input.trim_end_matches('/');
    ensure!(!trimmed.is_empty(), "SGPT_BASE_URL is not set.");
    let url = Url::parse(trimmed)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "SGPT_BASE_URL must start with http:// or https://."
    );
    let path = url.path().trim_end_matches('/');
    ensure!(
        !path.ends_with("/chat/completions"),
        "SGPT_BASE_URL must be a base URL, not a /chat/completions endpoint."
    );
    if path.ends_with("/v1") {
        Ok(format!("{trimmed}/chat/completions"))
    } else {
        Ok(format!("{trimmed}/v1/chat/completions"))
    }
}

pub fn validate_api_key(value: &str) -> anyhow::Result<()> {
    ensure!(!value.is_empty(), "SGPT_API_KEY is not set.");
    ensure!(
        !value.chars().any(char::is_control),
        "SGPT_API_KEY must not contain control characters or newlines."
    );
    Ok(())
}

pub fn parse_timeout(value: Option<String>) -> anyhow::Result<Duration> {
    let seconds = match value {
        Some(value) => value.parse::<u64>()?,
        None => 60,
    };
    ensure!(
        (1..=600).contains(&seconds),
        "SGPT_TIMEOUT_SECONDS must be in 1..=600."
    );
    Ok(Duration::from_secs(seconds))
}

pub fn parse_tunnel_port(value: Option<String>) -> anyhow::Result<Option<u16>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let port = value.parse::<u16>()?;
    ensure!(
        (1024..=65535).contains(&port),
        "SGPT_PORT must be in 1024..=65535."
    );
    Ok(Some(port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_base_url_to_chat_completions_endpoint() {
        assert_eq!(
            normalize_base_url("https://api.example.com").unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            normalize_base_url("https://api.example.com/v1/").unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn rejects_full_chat_completions_endpoint() {
        let err = normalize_base_url("https://api.example.com/v1/chat/completions")
            .unwrap_err()
            .to_string();
        assert!(err.contains("base URL"));
    }

    #[test]
    fn rejects_invalid_api_key_content() {
        assert!(validate_api_key("abc123").is_ok());
        assert!(validate_api_key("abc\n123").is_err());
        assert!(validate_api_key("").is_err());
    }

    #[test]
    fn parses_timeout_range() {
        assert_eq!(parse_timeout(None).unwrap(), Duration::from_secs(60));
        assert_eq!(
            parse_timeout(Some("1".into())).unwrap(),
            Duration::from_secs(1)
        );
        assert!(parse_timeout(Some("0".into())).is_err());
        assert!(parse_timeout(Some("601".into())).is_err());
    }
}
