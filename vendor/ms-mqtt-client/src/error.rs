// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error types for the MQTT client library.

use thiserror::Error;

pub use crate::client::token::completion::CompletionError; // Re-export to have all errors in one place

/// Indicates that a handle is detached from the client session that would process its request.
///
/// The request is not accepted and does not affect client state.
#[derive(Debug, Clone, Error)]
#[error("Communication channels have been closed")]
pub struct DetachedError {}

/// Indicates that the MQTT operation did not complete successfully
/// NOTE: Does NOT contain the reason code as an enum, as it must be agnostic to the operation type.
#[derive(Debug, Clone, Error)]
pub struct OperationFailure {
    pub reason: String,
}

impl From<String> for OperationFailure {
    fn from(value: String) -> Self {
        OperationFailure { reason: value }
    }
}

impl std::fmt::Display for OperationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Operation failed: {}", self.reason)
    }
}

/// Indicates a failure to connect to an MQTT server.
#[derive(Debug, Error)]
pub enum ConnectError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("I/O error: {0}")]
    Io(
        #[from]
        #[source]
        std::io::Error,
    ),
    #[error("connection rejected by server: {0:?}")]
    Rejected(crate::packet::ConnAck),
    #[error("timed out waiting for response packet")]
    ResponseTimeout,
}

/// Indicates a protocol violation of the MQTT specification
#[derive(Debug, Error)]
#[error(transparent)]
pub struct ProtocolError(#[from] ProtocolErrorRepr);

// LOCAL PATCH (mqtt-navigator): the app tells these two errors apart to show the right text.
impl ProtocolError {
    /// The server sent a PUBLISH with QoS 2. This client does not support QoS 2.
    pub fn is_unsupported_qos2(&self) -> bool {
        matches!(self.0, ProtocolErrorRepr::UnsupportedQoS2)
    }

    /// The server sent a PUBLISH with a packet identifier that is still in use, and the DUP flag
    /// was not set.
    pub fn is_duplicate_packet_id(&self) -> bool {
        matches!(self.0, ProtocolErrorRepr::DuplicatePacketIdentifier)
    }
}

#[derive(Debug, Error)]
pub(crate) enum ProtocolErrorRepr {
    #[error("protocol violation: malformed packet: {0}")]
    MalformedPacket(
        #[from]
        #[source]
        crate::mqtt_proto::DecodeError,
    ),
    #[error("protocol violation: unexpected packet")]
    UnexpectedPacket,
    #[error("protocol violation: missing or mismatched authentication method")]
    AuthenticationMethodMismatch,
    // LOCAL PATCH (mqtt-navigator)
    #[error(
        "protocol violation: the server sent a QoS 2 PUBLISH, which this client does not support"
    )]
    UnsupportedQoS2,
    // LOCAL PATCH (mqtt-navigator)
    #[error("protocol violation: PUBLISH reuses a packet identifier that is in use, without DUP")]
    DuplicatePacketIdentifier,
}
