//! Core logic of MQTT Navigator. This crate must not depend on gpui.
#![forbid(unsafe_code)]

pub mod model;
pub mod net;
pub mod store;

pub use model::{Incoming, MsgProps, Qos};
