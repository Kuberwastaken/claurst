//! `wreq`/BoringSSL client whose ClientHello matches the official Claude Code
//! client (Bun). Target JA4 `t13d1714h1_5b57614c22b0_7baf387fc6ff`, verified on
//! tls.peet.ws. ALPN is http/1.1 only — the client speaks HTTP/1.1 to the API,
//! so there is no HTTP/2 fingerprint to match. See `findings/TLS-IMPERSONATION.md`.

use std::time::Duration;
use wreq::tls::{AlpnProtocol, TlsOptions, TlsVersion};

/// Bun's 17 ciphers, in BoringSSL order.
const BUN_CIPHERS: &str = concat!(
    "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256:TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384:TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384:",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256:TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA:TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA:",
    "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA:TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA:",
    "TLS_RSA_WITH_AES_128_GCM_SHA256:TLS_RSA_WITH_AES_256_GCM_SHA384:",
    "TLS_RSA_WITH_AES_128_CBC_SHA:TLS_RSA_WITH_AES_256_CBC_SHA"
);

/// `TlsOptions` reproducing Bun's ClientHello.
pub fn bun_tls_options() -> TlsOptions {
    TlsOptions::builder()
        .cipher_list(BUN_CIPHERS)
        .curves_list("X25519:P-256:P-384")
        .alpn_protocols([AlpnProtocol::HTTP1])
        .min_tls_version(TlsVersion::TLS_1_2)
        .max_tls_version(TlsVersion::TLS_1_3)
        .session_ticket(true)
        .enable_ocsp_stapling(true)
        .enable_signed_cert_timestamps(true)
        .enable_ech_grease(true)
        .grease_enabled(false)
        .pre_shared_key(false)
        .build()
}

/// `wreq` client with Bun's TLS fingerprint, for Anthropic calls.
pub fn build_anthropic_client(timeout: Duration) -> wreq::Result<wreq::Client> {
    wreq::Client::builder()
        .tls_options(bun_tls_options())
        .timeout(timeout)
        .build()
}
