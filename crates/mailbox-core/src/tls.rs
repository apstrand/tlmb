//! TLS client setup: system roots, with a portable fallback.
//!
//! §9 wants the platform's trust store. `rustls-native-certs` reads that store
//! on macOS, Windows, and generic Unix. iOS is Unix as far as that crate is
//! concerned, but there is no `/etc/ssl/certs` on a phone — the roots live in
//! the Keychain — so a native load there yields nothing. When that happens we
//! fall back to the Mozilla bundle. A device with a working system store never
//! hits the fallback; the fallback is how mobile still verifies certificates.

use std::net::TcpStream;
use std::sync::Arc;

use crate::error::{Error, Result};

/// A TLS-wrapped TCP stream, used by both IMAP and SMTP.
pub type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

fn ensure_provider() {
    // `default-features = false` on rustls means nobody installed a provider
    // for us. Installing twice is a no-op: the first caller wins.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The trust store §9 describes, plus the fallback iOS needs.
pub fn root_store() -> Result<rustls::RootCertStore> {
    ensure_provider();
    let mut roots = rustls::RootCertStore::empty();
    #[cfg(not(target_arch = "wasm32"))]
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    if roots.is_empty() {
        return Err(Error::Network(
            "no system trust roots were readable, so no certificate can be verified".into(),
        ));
    }
    Ok(roots)
}

pub fn client_config() -> Result<rustls::ClientConfig> {
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(root_store()?)
        .with_no_client_auth())
}

/// Starts a verified TLS client connection without choosing its byte stream.
/// Native IMAP/SMTP attach this to TCP; hosted mode drives the same state
/// machine with ciphertext received from its WebSocket relay.
pub fn client_connection(host: &str) -> Result<rustls::ClientConnection> {
    let config = client_config()?;
    let server_name = rustls_pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| Error::usage(format!("`{host}` is not a valid server name")))?;
    rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| Error::Network(format!("starting TLS with {host}: {e}")))
}

/// Wraps `tcp` in TLS for `host`, using the roots of [`root_store`].
pub fn stream(host: &str, tcp: TcpStream) -> Result<TlsStream> {
    let client = client_connection(host)?;
    Ok(rustls::StreamOwned::new(client, tcp))
}
