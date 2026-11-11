//! Test helpers: a key pair, a signed manifest and a local HTTP server (std only).

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;

pub struct Keys {
    pub pubkey: String,
    pk: minisign::PublicKey,
    sk: minisign::SecretKey,
}

pub fn keys() -> Keys {
    let kp = minisign::KeyPair::generate_unencrypted_keypair().expect("keys");
    Keys {
        pubkey: kp.pk.to_box().expect("box").to_string(),
        pk: kp.pk,
        sk: kp.sk,
    }
}

impl Keys {
    pub fn sign(&self, data: &[u8]) -> String {
        minisign::sign(
            Some(&self.pk),
            &self.sk,
            Cursor::new(data),
            Some("test"),
            Some("test key"),
        )
        .expect("sign")
        .to_string()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `latest.json` text for one target.
pub fn manifest(version: &str, target: &str, url: &str, bytes: &[u8]) -> String {
    manifest_with(version, target, url, &sha256_hex(bytes), bytes.len() as u64)
}

pub fn manifest_with(version: &str, target: &str, url: &str, sha: &str, size: u64) -> String {
    format!(
        r#"{{"version":"{version}","targets":{{"{target}":{{"url":"{url}","sha256":"{sha}","size":{size}}}}}}}"#
    )
}

/// A route: the body and, optionally, how many bytes to send before the server closes the
/// connection (the header still promises the whole body).
pub struct Route {
    pub body: Vec<u8>,
    pub cut: Option<usize>,
}

pub fn route(body: impl Into<Vec<u8>>) -> Route {
    Route {
        body: body.into(),
        cut: None,
    }
}

/// Serves `routes` on 127.0.0.1 until the test process ends. Returns the base URL
/// (`http://127.0.0.1:PORT`).
/// `routes` gets the base URL, because a manifest holds the address of its artifact.
pub fn serve(routes: impl FnOnce(&str) -> HashMap<String, Route>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let routes = routes(&base);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if s.read(&mut b).unwrap_or(0) == 0 {
                    break;
                }
                head.push(b[0]);
            }
            let text = String::from_utf8_lossy(&head);
            let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
            let Some(r) = routes.get(&path) else {
                let _ = s.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            };
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                r.body.len()
            );
            let n = r.cut.unwrap_or(r.body.len());
            let _ = s.write_all(&r.body[..n]);
        }
    });
    base
}
