//! Opt-in triage logging to stderr. Enable with WL_MENU_DEBUG=1.

const LOG_PREFIX: &str = "wl-menu";

/// Whether debug lines are printed (default: off).
pub fn enabled() -> bool {
    match std::env::var("WL_MENU_DEBUG") {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => false,
    }
}

/// Print one debug line when triage logging is enabled.
pub fn log(msg: impl AsRef<str>) {
    if enabled() {
        eprintln!("{LOG_PREFIX}: debug: {}", msg.as_ref());
    }
}
