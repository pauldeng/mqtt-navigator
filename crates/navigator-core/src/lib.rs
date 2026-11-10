//! Core logic of MQTT Navigator. This crate must not depend on gpui.
#![forbid(unsafe_code)]

/// Quality of service. QoS 2 is not expressible: the MQTT library does not support it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Qos {
    #[default]
    AtMostOnce,
    AtLeastOnce,
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
