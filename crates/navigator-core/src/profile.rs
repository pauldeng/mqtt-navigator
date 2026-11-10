//! Connection profiles. A profile has no secrets: passwords and key passphrases live in the OS
//! keyring (the app) or in memory (`net::ConnectConfig`). Certificates are stored as paths.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::Qos;
use crate::net::ConnectConfig;
use crate::topic;

/// Transport. TLS is the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Tls,
    Tcp,
    Wss,
    Ws,
}

impl Transport {
    pub fn default_port(self) -> u16 {
        match self {
            Self::Tls => 8883,
            Self::Tcp => 1883,
            Self::Wss => 443,
            Self::Ws => 80,
        }
    }

    pub fn is_encrypted(self) -> bool {
        matches!(self, Self::Tls | Self::Wss)
    }

    pub fn is_websocket(self) -> bool {
        matches!(self, Self::Ws | Self::Wss)
    }
}

/// The last-will message. QoS is 0 or 1 (the type cannot hold 2).
///
/// In JSON the payload is a lowercase hex string (for example `"6f66666c696e65"`). Hex is exact
/// for any bytes, and it is the same form as the Hex payload mode of the composer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Will {
    pub topic: String,
    #[serde(with = "hex")]
    pub payload: Vec<u8>,
    pub qos: Qos,
    pub retain: bool,
    /// MQTT 5 Will Delay Interval.
    pub delay_secs: u32,
}

mod hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(out, "{b:02x}");
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        let bad = || serde::de::Error::custom("the Will payload must be a hex string");
        if !text.is_ascii() || text.len() % 2 != 0 {
            return Err(bad());
        }
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| bad()))
            .collect()
    }
}

/// One saved connection. Missing JSON fields take their defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub id: Uuid,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub transport: Transport,
    /// WebSocket path, for example `/mqtt`.
    pub ws_path: String,
    /// Empty: the broker assigns a client ID.
    pub client_id: String,
    pub username: Option<String>,
    /// Extra CA certificates (PEM file path), added to the system roots.
    pub ca_file: Option<PathBuf>,
    /// Client certificate chain (PEM file path), for mTLS. Needs `client_key`.
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    /// Verify the server certificate and host name.
    pub verify_tls: bool,
    pub clean_start: bool,
    pub session_expiry_secs: u32,
    pub keepalive_secs: u16,
    pub will: Option<Will>,
    /// Filters to subscribe after each connect.
    pub subscriptions: Vec<(String, Qos)>,
    /// Filters that the broker may still hold for this profile's persistent session (session
    /// expiry above 0). The net thread reports them (`Event::SessionFilters`). The app saves them
    /// and gives them back at the next start, so a removal is not lost across a restart. It is
    /// state, not a setting: an export and an import leave it out.
    pub session_filters: Vec<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self::new()
    }
}

/// The longest WebSocket path that the app accepts.
const MAX_WS_PATH: usize = 1024;

/// A wrong field of a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    /// `name`, `host`, `port`, `ws_path`, `username`, `client_id`, `will.topic` or
    /// `subscriptions`.
    pub field: &'static str,
    pub message: String,
}

fn err(field: &'static str, message: impl Into<String>) -> FieldError {
    FieldError {
        field,
        message: message.into(),
    }
}

impl Profile {
    /// A new profile with the defaults of the connection sheet: TLS on port 8883, and `#`.
    pub fn new() -> Self {
        let id = Uuid::new_v4();
        Self {
            id,
            name: String::new(),
            host: "localhost".into(),
            port: Transport::Tls.default_port(),
            transport: Transport::Tls,
            ws_path: "/mqtt".into(),
            client_id: format!("mqtt-navigator-{}", &id.simple().to_string()[..8]),
            username: None,
            ca_file: None,
            client_cert: None,
            client_key: None,
            verify_tls: true,
            clean_start: true,
            session_expiry_secs: 0,
            keepalive_secs: 60,
            will: None,
            subscriptions: vec![("#".into(), Qos::AtMostOnce)],
            session_filters: Vec::new(),
        }
    }

    /// Takes `session_filters` from `old`, the saved version of this profile. A change of host,
    /// port or client ID is another broker session: the filters are cleared.
    pub fn carry_session_from(&mut self, old: &Profile) {
        let same =
            (&self.host, self.port, &self.client_id) == (&old.host, old.port, &old.client_id);
        self.session_filters = if same {
            old.session_filters.clone()
        } else {
            Vec::new()
        };
    }

    /// Changes the transport. The port follows only if it is the default of the old transport.
    pub fn set_transport(&mut self, transport: Transport) {
        if self.port == self.transport.default_port() {
            self.port = transport.default_port();
        }
        self.transport = transport;
    }

    /// Checks every field. `others` are the other saved profiles (this one may be among them: a
    /// profile with the same ID is skipped).
    pub fn validate(&self, others: &[Profile]) -> Result<(), Vec<FieldError>> {
        let mut errors = Vec::new();
        let name = self.name.trim();
        if name.is_empty() {
            errors.push(err("name", "Enter a name."));
        } else if others
            .iter()
            .any(|o| o.id != self.id && o.name.trim().to_lowercase() == name.to_lowercase())
        {
            errors.push(err("name", "Another connection has this name."));
        }
        if self.host.trim().is_empty() {
            errors.push(err("host", "Enter a host name or an IP address."));
        }
        if self.port == 0 {
            errors.push(err("port", "The port must be from 1 to 65535."));
        }
        if self.transport.is_websocket() && !self.ws_path.starts_with('/') {
            errors.push(err("ws_path", "The WebSocket path must start with /."));
        } else if self.ws_path.len() > MAX_WS_PATH || self.ws_path.chars().any(char::is_control) {
            errors.push(err("ws_path", "The WebSocket path is not valid."));
        }
        // MQTT strings and binary data have a 2-byte length. A longer value makes the MQTT
        // library panic, so it must never reach a connect.
        if let Some(u) = &self.username
            && (u.len() > topic::MAX_LEN || u.contains('\0'))
        {
            errors.push(err(
                "username",
                "The user name is longer than 65535 bytes or has a NUL character.",
            ));
        }
        if self.client_id.len() > topic::MAX_LEN || self.client_id.contains('\0') {
            errors.push(err("client_id", "The client ID is not valid."));
        }
        if self.client_cert.is_some() != self.client_key.is_some() {
            errors.push(err(
                "client_cert",
                "Set both the client certificate and the client key, or neither.",
            ));
        }
        if let Some(w) = &self.will
            && (w.topic.is_empty()
                || w.topic.len() > topic::MAX_LEN
                || w.topic.contains(['+', '#', '\0']))
        {
            errors.push(err(
                "will.topic",
                "The Will topic must be a topic name without + or #.",
            ));
        }
        if let Some(w) = &self.will
            && w.payload.len() > topic::MAX_LEN
        {
            errors.push(err(
                "will.topic",
                "The Will payload is longer than 65535 bytes.",
            ));
        }
        if let Some((f, _)) = self
            .subscriptions
            .iter()
            .find(|(f, _)| !topic::is_valid_filter(f))
        {
            errors.push(err(
                "subscriptions",
                format!("\"{f}\" is not a valid topic filter."),
            ));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl ConnectConfig {
    /// Checks what a connect needs: the profile and the secrets. The text is for the user.
    /// A password longer than 65535 bytes would make the MQTT library panic.
    pub fn validate(&self) -> Result<(), String> {
        if self
            .password
            .as_ref()
            .is_some_and(|p| p.len() > topic::MAX_LEN)
        {
            return Err("The password is longer than 65535 bytes.".into());
        }
        self.profile.validate(&[]).map_err(|e| {
            e.first()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "The connection is not valid.".into())
        })
    }
}

/// Gives each profile of `new` a name that no profile of `existing` (or earlier in `new`) has.
/// A clash gets the suffix " (2)", " (3)" and so on. Names are compared without case.
pub fn dedupe_names(new: &mut [Profile], existing: &[Profile]) {
    let mut taken: HashSet<String> = existing
        .iter()
        .map(|p| p.name.trim().to_lowercase())
        .collect();
    for p in new {
        let base = p.name.trim().to_owned();
        let mut name = base.clone();
        let mut n = 2;
        while !base.is_empty() && taken.contains(&name.to_lowercase()) {
            name = format!("{base} ({n})");
            n += 1;
        }
        p.name = name;
        taken.insert(p.name.to_lowercase());
    }
}

const FORMAT: &str = "mqtt-navigator-profiles";
const VERSION: u32 = 1;

#[derive(Serialize)]
struct ExportRef<'a> {
    format: &'static str,
    version: u32,
    profiles: &'a [Profile],
}

#[derive(Deserialize)]
struct Export {
    format: String,
    version: u32,
    profiles: Vec<Profile>,
}

/// Versioned JSON of profiles. It has no secrets and no certificate contents (paths only).
pub fn export_profiles(profiles: &[Profile]) -> String {
    let clean: Vec<Profile> = profiles
        .iter()
        .map(|p| Profile {
            session_filters: Vec::new(),
            ..p.clone()
        })
        .collect();
    let doc = ExportRef {
        format: FORMAT,
        version: VERSION,
        profiles: &clean,
    };
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// Reads an export. Returns new profiles with fresh IDs. A name that clashes with `existing` (or
/// with an earlier imported profile) gets the suffix " (2)", " (3)" and so on. Nothing is
/// overwritten. One invalid profile fails the whole import.
pub fn import_profiles(json: &str, existing: &[Profile]) -> Result<Vec<Profile>, String> {
    let doc: Export =
        serde_json::from_str(json).map_err(|e| format!("This is not a profile export: {e}"))?;
    if doc.format != FORMAT {
        return Err("This is not a profile export.".into());
    }
    if doc.version > VERSION {
        return Err(format!(
            "This export has version {}. This app reads version {VERSION} and older.",
            doc.version
        ));
    }
    let mut out = Vec::with_capacity(doc.profiles.len());
    for mut p in doc.profiles {
        p.id = Uuid::new_v4();
        p.session_filters.clear();
        p.validate(&[]).map_err(|e| {
            let shown = if p.name.trim().is_empty() {
                "(no name)"
            } else {
                &p.name
            };
            let first = e.first().map(|e| e.message.as_str()).unwrap_or_default();
            format!("Profile {shown}: {first}")
        })?;
        out.push(p);
    }
    dedupe_names(&mut out, existing);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Profile {
        Profile {
            name: name.into(),
            ..Profile::new()
        }
    }

    fn fields(p: &Profile, others: &[Profile]) -> Vec<&'static str> {
        p.validate(others)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|e| e.field)
            .collect()
    }

    #[test]
    fn defaults_follow_the_ui_design() {
        let p = Profile::new();
        assert_eq!(
            (p.transport, p.port, p.verify_tls),
            (Transport::Tls, 8883, true)
        );
        assert!(p.clean_start && p.session_expiry_secs == 0 && p.keepalive_secs == 60);
        assert!(p.client_id.starts_with("mqtt-navigator-") && p.client_id.len() == 23);
        assert_eq!(p.subscriptions, vec![("#".to_owned(), Qos::AtMostOnce)]);
        assert_ne!(p.id, Profile::new().id);
    }

    #[test]
    fn serde_round_trip_and_missing_fields() {
        let mut p = named("a");
        p.will = Some(Will {
            topic: "w".into(),
            payload: vec![0, 255, b'x'],
            qos: Qos::AtLeastOnce,
            retain: true,
            delay_secs: 5,
        });
        p.client_cert = Some("/c.pem".into());
        p.client_key = Some("/k.pem".into());
        let json = serde_json::to_string(&p).expect("json");
        assert!(json.contains("\"00ff78\""));
        assert_eq!(serde_json::from_str::<Profile>(&json).expect("back"), p);
        // Missing fields take defaults.
        let q: Profile = serde_json::from_str(r#"{"name":"b","host":"h"}"#).expect("partial");
        assert_eq!(
            (q.port, q.transport, q.keepalive_secs),
            (8883, Transport::Tls, 60)
        );
        // QoS 2 is refused.
        assert!(serde_json::from_str::<Profile>(r#"{"subscriptions":[["a",2]]}"#).is_err());
        assert!(serde_json::from_str::<Will>(r#"{"qos":2}"#).is_err());
    }

    #[test]
    fn set_transport_moves_only_a_default_port() {
        let mut p = Profile::new();
        p.set_transport(Transport::Tcp);
        assert_eq!(p.port, 1883);
        p.set_transport(Transport::Wss);
        assert_eq!(p.port, 443);
        p.port = 9001;
        p.set_transport(Transport::Ws);
        assert_eq!((p.transport, p.port), (Transport::Ws, 9001));
    }

    #[test]
    fn validate_cases() {
        let ok = named("prod");
        assert!(ok.validate(&[]).is_ok());
        // The same ID is not a clash. Another ID with the same name in other case is.
        assert!(ok.validate(std::slice::from_ref(&ok)).is_ok());
        assert_eq!(fields(&ok, &[named(" PROD ")]), ["name"]);
        assert_eq!(fields(&named("  "), &[]), ["name"]);
        let bad = Profile {
            host: " ".into(),
            port: 0,
            transport: Transport::Ws,
            ws_path: "mqtt".into(),
            client_cert: Some("c".into()),
            will: Some(Will {
                topic: "a/#".into(),
                ..Will::default()
            }),
            subscriptions: vec![("a/#/b".into(), Qos::AtMostOnce)],
            ..named("x")
        };
        assert_eq!(
            fields(&bad, &[]),
            [
                "host",
                "port",
                "ws_path",
                "client_cert",
                "will.topic",
                "subscriptions"
            ]
        );
        // A hidden WebSocket path is not checked for TLS.
        let tls = Profile {
            ws_path: String::new(),
            ..named("t")
        };
        assert!(tls.validate(&[]).is_ok());
    }

    #[test]
    fn export_has_no_secrets_and_import_never_overwrites() {
        let mut a = named("Site");
        a.username = Some("bob".into());
        a.ca_file = Some("/etc/ca.pem".into());
        let json = export_profiles(&[a.clone(), named("Other")]);
        assert!(json.contains("\"format\": \"mqtt-navigator-profiles\""));
        assert!(!json.to_lowercase().contains("password"));
        let existing = vec![a.clone()];
        let got = import_profiles(&json, &existing).expect("import");
        let names: Vec<_> = got.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Site (2)", "Other"]);
        assert!(got.iter().all(|p| p.id != a.id));
        assert_ne!(got[0].id, got[1].id);
        // Importing again into the grown list gives " (3)".
        let all: Vec<_> = existing.into_iter().chain(got).collect();
        let again = import_profiles(&json, &all).expect("again");
        assert_eq!(again[0].name, "Site (3)");
        assert_eq!(again[1].name, "Other (2)");
    }

    #[test]
    fn import_rejects_bad_input() {
        assert!(import_profiles("nonsense", &[]).is_err());
        assert!(import_profiles(r#"{"format":"x","version":1,"profiles":[]}"#, &[]).is_err());
        let future = r#"{"format":"mqtt-navigator-profiles","version":2,"profiles":[]}"#;
        assert!(import_profiles(future, &[]).is_err());
        let bad = r#"{"format":"mqtt-navigator-profiles","version":1,"profiles":[{"name":"a","port":0}]}"#;
        assert!(
            import_profiles(bad, &[])
                .expect_err("invalid")
                .contains("port")
        );
    }

    #[test]
    fn mqtt_field_lengths_are_checked() {
        let max = "a".repeat(65_535);
        let long = "a".repeat(65_536);
        let user = |u: &str| Profile {
            username: Some(u.into()),
            ..named("u")
        };
        assert!(user(&max).validate(&[]).is_ok());
        assert_eq!(fields(&user(&long), &[]), ["username"]);
        assert_eq!(fields(&user("a\0b"), &[]), ["username"]);
        let will = |payload: usize| Profile {
            will: Some(Will {
                topic: "w".into(),
                payload: vec![0; payload],
                ..Will::default()
            }),
            ..named("w")
        };
        assert!(will(65_535).validate(&[]).is_ok());
        assert_eq!(fields(&will(65_536), &[]), ["will.topic"]);
        let path = |p: String| Profile {
            ws_path: p,
            ..named("p")
        };
        assert_eq!(
            fields(&path(format!("/{}", "a".repeat(2000))), &[]),
            ["ws_path"]
        );
        assert_eq!(fields(&path("/a\nb".into()), &[]), ["ws_path"]);
        let id = Profile {
            client_id: long,
            ..named("c")
        };
        assert_eq!(fields(&id, &[]), ["client_id"]);
    }

    #[test]
    fn connect_config_checks_the_password_and_the_profile() {
        let mut c = ConnectConfig::from(named("x"));
        assert!(c.validate().is_ok());
        c.password = Some("a".repeat(65_535));
        assert!(c.validate().is_ok());
        c.password = Some("a".repeat(65_536));
        let why = c.validate().expect_err("too long");
        assert!(why.contains("password"));
        // The text never holds the secret.
        assert!(!why.contains("aaaa"));
        c.password = None;
        c.profile.port = 0;
        assert!(c.validate().expect_err("port").contains("port"));
    }

    #[test]
    fn dedupe_names_runs_against_the_current_list() {
        let mut new = vec![named("Site"), named("Site"), named("")];
        dedupe_names(&mut new, &[named("site")]);
        let names: Vec<_> = new.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Site (2)", "Site (3)", ""]);
    }

    #[test]
    fn session_filters_default_empty_and_follow_the_session() {
        let q: Profile = serde_json::from_str(r#"{"name":"b"}"#).expect("old file");
        assert!(q.session_filters.is_empty());
        let mut old = named("a");
        old.session_filters = vec!["x/#".into()];
        let json = serde_json::to_string(&old).expect("json");
        assert!(json.contains("\"session_filters\":[\"x/#\"]"));
        // The same host, port and client ID: the saved filters stay, whatever the sheet had.
        let mut edited = Profile {
            name: "renamed".into(),
            ..old.clone()
        };
        edited.session_filters.clear();
        edited.carry_session_from(&old);
        assert_eq!(edited.session_filters, old.session_filters);
        // Another host, port or client ID is another broker session.
        for change in [
            |p: &mut Profile| p.host = "other".into(),
            |p: &mut Profile| p.port += 1,
            |p: &mut Profile| p.client_id = "other".into(),
        ] {
            let mut p = old.clone();
            change(&mut p);
            p.carry_session_from(&old);
            assert!(p.session_filters.is_empty());
        }
        // Export and import leave the state out.
        let json = export_profiles(std::slice::from_ref(&old));
        assert!(!json.contains("x/#"));
        let mut with = old.clone();
        with.name = "imp".into();
        let doc = serde_json::json!({"format": FORMAT, "version": 1, "profiles": [with]});
        let got = import_profiles(&doc.to_string(), &[]).expect("import");
        assert!(got[0].session_filters.is_empty());
    }
}
