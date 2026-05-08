pub fn enabled() -> bool {
    std::env::var("SGPT_DEBUG").ok().as_deref() == Some("1")
}

pub fn log(key: &str, value: impl std::fmt::Display) {
    if enabled() {
        eprintln!("{key}={value}");
    }
}
