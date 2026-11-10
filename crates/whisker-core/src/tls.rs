//! How servers' certificates are checked.
//!
//! On desktops the system checks them (its own list of trusted
//! authorities). On Android that needs a Java helper and a setup call this
//! kind of app (no Java code) doesn't have, and without them the first
//! secure connection panicked: signing in hung forever. So on Android the
//! certificates are checked in the app, against Mozilla's list of trusted
//! authorities (the one Firefox uses), which is built in. Authorities a
//! user added to the phone themself aren't trusted there.

/// Mozilla's trusted authorities, for [`matrix_sdk::ClientBuilder::add_root_certificates`].
#[cfg(target_os = "android")]
pub fn roots() -> Vec<reqwest::Certificate> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().filter_map(|c| reqwest::Certificate::from_der(c.as_ref()).ok()).collect()
}

/// Starts an HTTP client that checks certificates the way that works here.
/// Every client the app makes itself starts from this.
pub fn http_builder() -> reqwest::ClientBuilder {
    let b = reqwest::Client::builder();
    #[cfg(target_os = "android")]
    let b = b.tls_certs_only(roots());
    b
}
