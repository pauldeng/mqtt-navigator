//! Data types shared by the net thread, the store and the UI.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;

/// Quality of service. QoS 2 is not expressible: the MQTT library does not support it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Qos {
    #[default]
    AtMostOnce,
    AtLeastOnce,
}

impl Qos {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::AtMostOnce => 0,
            Self::AtLeastOnce => 1,
        }
    }
}

impl TryFrom<u8> for Qos {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::AtMostOnce),
            1 => Ok(Self::AtLeastOnce),
            other => Err(other),
        }
    }
}

/// MQTT 5 properties of a message that the UI shows or sends.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MsgProps {
    pub content_type: Option<String>,
    pub response_topic: Option<String>,
    pub correlation_data: Option<Bytes>,
    pub message_expiry: Option<u32>,
    pub user: Vec<(String, String)>,
    /// Payload format indicator is "UTF-8".
    pub payload_utf8: bool,
}

impl MsgProps {
    /// Approximate heap bytes, for the memory budget.
    pub fn heap_bytes(&self) -> usize {
        let s = |o: &Option<String>| o.as_ref().map_or(0, String::len);
        s(&self.content_type)
            + s(&self.response_topic)
            + self.correlation_data.as_ref().map_or(0, Bytes::len)
            + self
                .user
                .iter()
                .map(|(k, v)| k.len() + v.len() + 48)
                .sum::<usize>()
    }
}

/// One received message, as the net thread hands it to the UI.
#[derive(Clone, Debug)]
pub struct Incoming {
    /// Connection generation. The UI drops messages of an old generation.
    pub generation: u64,
    pub topic: Arc<str>,
    pub payload: Bytes,
    pub qos: Qos,
    pub retain: bool,
    pub props: MsgProps,
    pub received_at: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::Qos;

    #[test]
    fn qos_2_is_rejected() {
        assert_eq!(Qos::try_from(0), Ok(Qos::AtMostOnce));
        assert_eq!(Qos::try_from(1), Ok(Qos::AtLeastOnce));
        assert_eq!(Qos::try_from(2), Err(2));
    }
}
