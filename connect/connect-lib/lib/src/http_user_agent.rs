//! Shared [`User-Agent`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/User-Agent) for
//! outbound HTTP from Datum Connect Plugin (reqwest and kube). Helps backend logs and support correlate
//! traffic with app builds.
//!
//! The version is [`env!("CARGO_PKG_VERSION")`] for this crate.

/// Product token plus version, OS, and CPU arch for support and debugging.
pub fn datum_http_user_agent() -> String {
    format!(
        "Datum Connect Plugin/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}
