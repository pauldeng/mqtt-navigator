//! TLS set-up and diagnostics.
//!
//! We build the OpenSSL connector here, so we can record why a certificate failed. The vendored
//! client turns the handshake error into a generic I/O error. The verify callback sees the
//! exact OpenSSL code first. We keep it in a [`TlsDiag`] and map it to plain text.
//!
//! The vendored client builds the connection with `connector.build().configure()` and
//! `into_ssl(hostname)`. That call also sets the host name (or IP address) to verify. A wrong
//! name gives code 62 or 64, and the unit tests below prove that.

use std::path::Path;
use std::sync::{Arc, Mutex};

use ms_mqtt_client::transport::TlsConfig;
use openssl::error::ErrorStack;
use openssl::ssl::{SslConnector, SslConnectorBuilder, SslMethod, SslVerifyMode, SslVersion};
use openssl::x509::X509;

/// First certificate problem that one connection attempt saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyFailure {
    /// OpenSSL `X509_V_ERR_*` code.
    pub code: i32,
    /// OpenSSL text for the code.
    pub text: String,
    /// "Not after" date of the certificate that failed, if OpenSSL gave one.
    pub not_after: Option<String>,
}

/// Shared slot that the verify callback fills. One for each connection attempt.
#[derive(Clone, Debug, Default)]
pub struct TlsDiag(Arc<Mutex<Option<VerifyFailure>>>);

impl TlsDiag {
    pub(crate) fn record(&self, failure: VerifyFailure) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // Keep the first failure. Later ones are follow-up errors.
        slot.get_or_insert(failure);
    }

    /// The first certificate problem, if the handshake saw one.
    pub fn failure(&self) -> Option<VerifyFailure> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// User text for the recorded problem.
    pub fn text(&self) -> Option<String> {
        self.failure().map(|f| verify_text(&f))
    }
}

// OpenSSL `X509_V_ERR_*` codes.
const UNABLE_TO_GET_ISSUER_CERT: i32 = 2;
const CERT_NOT_YET_VALID: i32 = 9;
const CERT_HAS_EXPIRED: i32 = 10;
const DEPTH_ZERO_SELF_SIGNED_CERT: i32 = 18;
const SELF_SIGNED_CERT_IN_CHAIN: i32 = 19;
const UNABLE_TO_GET_ISSUER_CERT_LOCALLY: i32 = 20;
const UNABLE_TO_VERIFY_LEAF_SIGNATURE: i32 = 21;
const HOSTNAME_MISMATCH: i32 = 62;
const IP_ADDRESS_MISMATCH: i32 = 64;

/// Plain text for a certificate problem.
pub fn verify_text(f: &VerifyFailure) -> String {
    match f.code {
        CERT_HAS_EXPIRED => match &f.not_after {
            Some(date) => format!(
                "The server certificate expired on {date}. Renew it, or turn off verification."
            ),
            None => "The server certificate expired. Renew it, or turn off verification.".into(),
        },
        CERT_NOT_YET_VALID => {
            "The server certificate is not valid yet. Check the clock of this computer and of the broker."
                .into()
        }
        DEPTH_ZERO_SELF_SIGNED_CERT
        | SELF_SIGNED_CERT_IN_CHAIN
        | UNABLE_TO_GET_ISSUER_CERT
        | UNABLE_TO_GET_ISSUER_CERT_LOCALLY
        | UNABLE_TO_VERIFY_LEAF_SIGNATURE => {
            "The server certificate is not trusted. Add its CA file, or turn off verification."
                .into()
        }
        HOSTNAME_MISMATCH | IP_ADDRESS_MISMATCH => {
            "The server certificate is for another host name.".into()
        }
        code => format!("TLS failed: {} (code {code}).", f.text),
    }
}

/// Builds the TLS configuration for one connection attempt.
///
/// This reads the PEM files, so call it on the net thread only. The error text is for the user.
pub fn build(
    ca_file: Option<&Path>,
    client_cert: Option<(&Path, &Path)>,
    verify: bool,
) -> Result<(TlsConfig, TlsDiag), String> {
    let (builder, diag) = connector(ca_file, client_cert, verify)?;
    Ok((TlsConfig::from(builder), diag))
}

const ENCRYPTED_KEY_TEXT: &str =
    "The client key is encrypted. Use a key without a passphrase (passphrase support comes later).";

/// Reads a private key. An encrypted key fails at once: OpenSSL must never ask for a passphrase
/// on the terminal, because that would block the net thread.
fn private_key_from_pem(pem: &[u8]) -> Result<openssl::pkey::PKey<openssl::pkey::Private>, String> {
    if pem.windows(9).any(|w| w == b"ENCRYPTED") {
        return Err(ENCRYPTED_KEY_TEXT.to_owned());
    }
    // Second line of defence: a callback that fails, so OpenSSL never prompts.
    openssl::pkey::PKey::private_key_from_pem_callback(pem, |_| Err(ErrorStack::get()))
        .map_err(|e| format!("The client key is not valid PEM: {e}"))
}

fn connector(
    ca_file: Option<&Path>,
    client_cert: Option<(&Path, &Path)>,
    verify: bool,
) -> Result<(SslConnectorBuilder, TlsDiag), String> {
    fn read(path: &Path, what: &str) -> Result<Vec<u8>, String> {
        std::fs::read(path)
            .map_err(|e| format!("Cannot read the {what} file {}: {e}", path.display()))
    }
    let tls = |e: ErrorStack| format!("TLS set-up failed: {e}");
    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(tls)?;
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(tls)?;
    if let Some(path) = ca_file {
        let certs = X509::stack_from_pem(&read(path, "CA")?)
            .map_err(|e| format!("The CA file {} is not valid PEM: {e}", path.display()))?;
        if certs.is_empty() {
            return Err(format!(
                "The CA file {} has no certificate.",
                path.display()
            ));
        }
        let store = builder.cert_store_mut();
        for cert in certs {
            store.add_cert(cert).map_err(tls)?;
        }
    }
    if let Some((cert_path, key_path)) = client_cert {
        let mut chain = X509::stack_from_pem(&read(cert_path, "client certificate")?)
            .map_err(|e| format!("The client certificate is not valid PEM: {e}"))?
            .into_iter();
        let cert = chain
            .next()
            .ok_or("The client certificate file has no certificate.")?;
        let key = private_key_from_pem(&read(key_path, "client key")?)?;
        builder.set_certificate(&cert).map_err(tls)?;
        // OpenSSL 3 can already refuse a wrong key in `set_private_key`.
        let mismatch = |_| "The client certificate and key do not match.".to_owned();
        builder.set_private_key(&key).map_err(mismatch)?;
        builder.check_private_key().map_err(mismatch)?;
        for extra in chain {
            builder.add_extra_chain_cert(extra).map_err(tls)?;
        }
    }
    let diag = TlsDiag::default();
    if verify {
        let slot = diag.clone();
        builder.set_verify_callback(SslVerifyMode::PEER, move |preverify_ok, ctx| {
            if !preverify_ok {
                let error = ctx.error();
                slot.record(VerifyFailure {
                    code: error.as_raw(),
                    text: error.error_string().to_owned(),
                    not_after: ctx.current_cert().map(|c| c.not_after().to_string()),
                });
            }
            // The result stays as OpenSSL computed it.
            preverify_ok
        });
    } else {
        builder.set_verify(SslVerifyMode::NONE);
    }
    Ok((builder, diag))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Certificates for tests. They are made in memory, so the tests need no files from outside.
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::pkey::{PKey, Private};
    use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
    use openssl::x509::{X509, X509NameBuilder};

    pub fn key() -> PKey<Private> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).expect("group");
        PKey::from_ec_key(EcKey::generate(&group).expect("ec key")).expect("pkey")
    }

    /// A certificate for `cn` (DNS name). `issuer` is `(cert, key)` of a CA, or `None` for a
    /// self-signed certificate. The validity is `[now + from_days, now + to_days]`.
    pub fn cert(
        cn: &str,
        key: &PKey<Private>,
        issuer: Option<(&X509, &PKey<Private>)>,
        ca: bool,
        from_days: i32,
        to_days: i32,
    ) -> X509 {
        let mut name = X509NameBuilder::new().expect("name");
        name.append_entry_by_nid(Nid::COMMONNAME, cn).expect("cn");
        let name = name.build();
        let mut b = X509::builder().expect("builder");
        b.set_version(2).expect("version");
        let serial = BigNum::from_u32(1)
            .and_then(|n| n.to_asn1_integer())
            .expect("serial");
        b.set_serial_number(&serial).expect("serial");
        b.set_subject_name(&name).expect("subject");
        b.set_issuer_name(issuer.map_or(&*name, |(c, _)| c.subject_name()))
            .expect("issuer");
        b.set_pubkey(key).expect("pubkey");
        b.set_not_before(&time(from_days)).expect("nb");
        b.set_not_after(&time(to_days)).expect("na");
        if ca {
            let bc = BasicConstraints::new().critical().ca().build().expect("bc");
            b.append_extension(bc).expect("ext");
        }
        let san = SubjectAlternativeName::new()
            .dns(cn)
            .build(&b.x509v3_context(issuer.map(|(c, _)| c.as_ref()), None))
            .expect("san");
        b.append_extension(san).expect("ext");
        b.sign(issuer.map_or(key, |(_, k)| k), MessageDigest::sha256())
            .expect("sign");
        b.build()
    }

    /// `Asn1Time` at `now + days` (days can be negative).
    fn time(days: i32) -> Asn1Time {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;
        Asn1Time::from_unix(now + i64::from(days) * 86_400).expect("asn1 time")
    }

    /// A temp folder that is removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "navigator-tls-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }

        pub fn write(&self, name: &str, data: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, data).expect("write");
            path
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use openssl::ssl::{SslAcceptor, SslMethod};
    use openssl::x509::X509;

    use super::testutil::*;
    use super::*;

    fn failure(code: i32, not_after: Option<&str>) -> VerifyFailure {
        VerifyFailure {
            code,
            text: "some text".into(),
            not_after: not_after.map(str::to_owned),
        }
    }

    #[test]
    fn encrypted_client_key_fails_at_once() {
        let key = key();
        let pem = key
            .private_key_to_pem_pkcs8_passphrase(openssl::symm::Cipher::aes_128_cbc(), b"secret")
            .expect("encrypted pem");
        assert!(String::from_utf8_lossy(&pem).contains("ENCRYPTED"));
        assert_eq!(
            private_key_from_pem(&pem).expect_err("must fail"),
            ENCRYPTED_KEY_TEXT
        );
        // The plain key still loads. Garbage is not valid PEM and never prompts.
        let plain = key.private_key_to_pem_pkcs8().expect("pem");
        assert!(private_key_from_pem(&plain).is_ok());
        assert!(
            private_key_from_pem(b"nonsense")
                .expect_err("must fail")
                .contains("not valid PEM")
        );
    }

    #[test]
    fn codes_map_to_plain_text() {
        assert_eq!(
            verify_text(&failure(CERT_HAS_EXPIRED, Some("Jan  1 00:00:00 2020 GMT"))),
            "The server certificate expired on Jan  1 00:00:00 2020 GMT. Renew it, or turn off verification."
        );
        assert!(verify_text(&failure(CERT_HAS_EXPIRED, None)).contains("expired."));
        assert!(verify_text(&failure(CERT_NOT_YET_VALID, None)).contains("not valid yet"));
        for code in [
            DEPTH_ZERO_SELF_SIGNED_CERT,
            SELF_SIGNED_CERT_IN_CHAIN,
            UNABLE_TO_GET_ISSUER_CERT_LOCALLY,
        ] {
            assert!(verify_text(&failure(code, None)).contains("not trusted"));
        }
        assert_eq!(
            verify_text(&failure(HOSTNAME_MISMATCH, None)),
            "The server certificate is for another host name."
        );
        assert_eq!(
            verify_text(&failure(IP_ADDRESS_MISMATCH, None)),
            "The server certificate is for another host name."
        );
        assert_eq!(
            verify_text(&failure(7, None)),
            "TLS failed: some text (code 7)."
        );
    }

    #[test]
    fn mismatched_client_key_is_reported() {
        let dir = TempDir::new();
        let (key_a, key_b) = (key(), key());
        let cert = cert("client", &key_a, None, false, -1, 1);
        let cert_path = dir.write("c.pem", &cert.to_pem().expect("pem"));
        let key_path = dir.write("k.pem", &key_b.private_key_to_pem_pkcs8().expect("pem"));
        let err = build(None, Some((&cert_path, &key_path)), true)
            .err()
            .expect("error");
        assert_eq!(err, "The client certificate and key do not match.");
        // The matching key is accepted.
        let key_path = dir.write("k2.pem", &key_a.private_key_to_pem_pkcs8().expect("pem"));
        assert!(build(None, Some((&cert_path, &key_path)), true).is_ok());
    }

    #[test]
    fn unreadable_and_empty_files_give_text() {
        let dir = TempDir::new();
        let err = build(Some(&dir.path().join("none.pem")), None, true)
            .err()
            .expect("error");
        assert!(err.starts_with("Cannot read the CA file"), "{err}");
        let empty = dir.write("e.pem", b"");
        let err = build(Some(&empty), None, true).err().expect("error");
        assert!(err.contains("no certificate"), "{err}");
    }

    /// Runs one TLS handshake against a local server and returns our diagnosis.
    /// `server_cert` is the leaf (with the chain after it), `host` is the name we connect to.
    fn handshake(
        server_cert: &[X509],
        server_key: &openssl::pkey::PKey<openssl::pkey::Private>,
        ca: Option<&X509>,
        host: &str,
        verify: bool,
    ) -> (bool, Option<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let mut acceptor =
            SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).expect("acceptor");
        acceptor.set_certificate(&server_cert[0]).expect("cert");
        for extra in &server_cert[1..] {
            acceptor.add_extra_chain_cert(extra.clone()).expect("chain");
        }
        acceptor.set_private_key(server_key).expect("key");
        let acceptor = acceptor.build();
        let server = std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                let _ = acceptor.accept(sock); // The client may abort.
            }
        });

        let dir = TempDir::new();
        let ca_path = ca.map(|c| dir.write("ca.pem", &c.to_pem().expect("pem")));
        let (builder, diag) = connector(ca_path.as_deref(), None, verify).expect("build");
        // Same steps as the vendored `tls_handshake`.
        let ssl = builder
            .build()
            .configure()
            .expect("configure")
            .into_ssl(host)
            .expect("ssl");
        let sock = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let ok = ssl.connect(sock).is_ok();
        server.join().expect("server");
        (ok, diag.text())
    }

    #[test]
    fn handshake_diagnoses_each_certificate_problem() {
        let (ca_key, srv_key) = (key(), key());
        let ca = cert("Test CA", &ca_key, None, true, -1, 30);
        let good = cert("localhost", &srv_key, Some((&ca, &ca_key)), false, -1, 30);

        // Trusted CA and matching name: works.
        let (ok, text) = handshake(
            std::slice::from_ref(&good),
            &srv_key,
            Some(&ca),
            "localhost",
            true,
        );
        assert!(ok, "{text:?}");
        assert_eq!(text, None);

        // Unknown CA.
        let (ok, text) = handshake(
            std::slice::from_ref(&good),
            &srv_key,
            None,
            "localhost",
            true,
        );
        assert!(!ok);
        assert!(text.expect("text").contains("not trusted"));

        // Wrong host name. This proves that the vendored `into_ssl` checks the name.
        let (ok, text) = handshake(
            std::slice::from_ref(&good),
            &srv_key,
            Some(&ca),
            "other.example",
            true,
        );
        assert!(!ok);
        assert_eq!(
            text.as_deref(),
            Some("The server certificate is for another host name.")
        );
        // Wrong IP address.
        let (ok, text) = handshake(
            std::slice::from_ref(&good),
            &srv_key,
            Some(&ca),
            "127.0.0.1",
            true,
        );
        assert!(!ok);
        assert!(text.expect("text").contains("another host name"));

        // Expired.
        let old = cert("localhost", &srv_key, Some((&ca, &ca_key)), false, -30, -2);
        let (ok, text) = handshake(&[old], &srv_key, Some(&ca), "localhost", true);
        assert!(!ok);
        let text = text.expect("text");
        assert!(
            text.starts_with("The server certificate expired on "),
            "{text}"
        );
        assert!(text.contains("GMT"), "{text}");

        // Not yet valid.
        let future = cert("localhost", &srv_key, Some((&ca, &ca_key)), false, 2, 30);
        let (ok, text) = handshake(&[future], &srv_key, Some(&ca), "localhost", true);
        assert!(!ok);
        assert!(text.expect("text").contains("not valid yet"));

        // Verification off: every problem is ignored and nothing is recorded.
        let (ok, text) = handshake(&[good], &srv_key, None, "other.example", false);
        assert!(ok);
        assert_eq!(text, None);
    }
}
