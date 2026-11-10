//! The text of "Copy diagnostics". It holds numbers and names of the app, the system and the
//! graphics adapter. It never holds a profile, a host, a path, a topic, a payload or a secret:
//! the type has no field for them.

use navigator_core::net::ConnState;
use navigator_core::store::StoreStats;

pub struct Info {
    pub version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    /// Adapter name and driver, or "Unavailable".
    pub gpu: String,
    pub state: &'static str,
    pub store: StoreStats,
    /// Messages that the net queues dropped.
    pub net_drops: u64,
    pub event_drops: u64,
}

impl Info {
    pub fn collect(
        gpu: String,
        state: &ConnState,
        store: StoreStats,
        net_drops: u64,
        event_drops: u64,
    ) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            gpu,
            state: state_label(state),
            store,
            net_drops,
            event_drops,
        }
    }

    pub fn text(&self) -> String {
        let s = &self.store;
        format!(
            "MQTT Navigator {}\n\
             System: {} {}\n\
             Graphics: {}\n\
             Connection: {}\n\
             Topics: {}\n\
             Messages: {}\n\
             Store: {} bytes used, {} bytes of history, {} list rows, {} list rows evicted\n\
             Dropped: {} messages (store), {} topics (store), {} messages (network), {} events\n",
            self.version,
            self.os,
            self.arch,
            self.gpu,
            self.state,
            s.topics,
            s.messages,
            s.used_bytes,
            s.history_bytes,
            s.list_rows,
            s.list_drops,
            s.drops,
            s.topic_drops,
            self.net_drops,
            self.event_drops,
        )
    }
}

/// The state without its reason: a reason can name a host.
fn state_label(s: &ConnState) -> &'static str {
    match s {
        ConnState::Disconnected => "Disconnected",
        ConnState::Connecting => "Connecting",
        ConnState::Connected => "Connected",
        ConnState::Reconnecting { .. } => "Reconnecting",
        ConnState::Disconnecting => "Disconnecting",
        ConnState::Failed(_) => "Failed",
    }
}

/// The adapter line. `None`: the platform does not tell.
pub fn gpu_text(specs: Option<(&str, &str, &str, bool)>) -> String {
    match specs {
        Some((device, driver, info, software)) => {
            let mut out = device.to_string();
            if !driver.is_empty() {
                out.push_str(&format!(", driver {driver}"));
            }
            if !info.is_empty() {
                out.push_str(&format!(" {info}"));
            }
            if software {
                out.push_str(" (software)");
            }
            out
        }
        None => "Unavailable".into(),
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro in the parent.
    use super::{Info, gpu_text};
    use navigator_core::Incoming;
    use navigator_core::net::{ConnState, Profile};
    use navigator_core::store::{Limits, TopicStore};

    #[test]
    fn the_text_holds_no_profile_data() {
        let p = Profile {
            host: "secret-broker.example.com".into(),
            username: Some("alice".into()),
            subscriptions: vec![("factory/line1/#".into(), navigator_core::Qos::AtMostOnce)],
            ..Profile::default()
        };
        let password = "hunter2";
        let state = ConnState::Failed(format!("cannot reach {}", p.host));
        // A store that holds a message with a topic and a payload.
        let mut store = TopicStore::new(Limits::default());
        store.insert(Incoming {
            generation: 1,
            topic: "factory/line1/temp".into(),
            payload: bytes::Bytes::from_static(b"hunter2"),
            qos: navigator_core::Qos::AtMostOnce,
            retain: false,
            props: navigator_core::MsgProps::default(),
            received_at: std::time::SystemTime::now(),
        });
        let info = Info::collect(
            gpu_text(Some(("Test GPU", "drv", "1.0", false))),
            &state,
            store.stats(),
            2,
            0,
        );
        let text = info.text();
        for secret in [
            p.host.as_str(),
            "alice",
            password,
            "factory",
            "line1",
            "temp",
            "cannot reach",
        ] {
            assert!(!text.contains(secret), "{secret}");
        }
        assert!(
            text.contains("Topics: 3") && text.contains("Messages: 1"),
            "{text}"
        );
        assert!(text.contains("Test GPU") && text.contains("Connection: Failed"));
        assert!(text.contains("2 messages (network)"));
        assert_eq!(gpu_text(None), "Unavailable");
    }
}
