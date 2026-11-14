//! A test publisher that uses `Net`, the same code path as the app. It has no subscription.
#![allow(dead_code)] // Each test crate uses a part of it.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use navigator_core::model::{MsgProps, Qos};
use navigator_core::net::{
    Command, ConnState, ConnectConfig, Event, Net, NetLimits, Outgoing, Profile, Protocol,
    PublishResult, SendError, Transport,
};

pub struct Publisher {
    pub net: Net,
    next_op: u64,
    /// The QoS of each operation that has no result yet.
    pending: HashMap<u64, Qos>,
    /// Messages that `Net::send` accepted.
    pub accepted: u64,
    /// Results that match the QoS: `Sent` for QoS 0, `Acknowledged` for QoS 1.
    pub done: u64,
    /// Any other result, and a result that does not match the QoS (for example `Sent` for QoS 1).
    pub failed: Vec<PublishResult>,
    /// Events `Reconnected`.
    pub reconnects: u64,
}

impl Publisher {
    /// Connects to `host:port` over MQTT 5 on plain TCP with a clean session.
    pub async fn connect(host: &str, port: u16) -> Result<Self, String> {
        let mut net = Net::start(NetLimits::default());
        net.send(Command::Connect(Box::new(ConnectConfig::from(Profile {
            host: host.into(),
            port,
            transport: Transport::Tcp,
            subscriptions: vec![],
            protocol: Protocol::V5,
            ..Profile::default()
        }))))
        .map_err(|e| format!("publisher connect: {e}"))?;
        tokio::time::timeout(
            Duration::from_secs(10),
            net.status.wait_for(|s| s.state == ConnState::Connected),
        )
        .await
        .map_err(|_| "publisher connect: timeout".to_owned())?
        .map_err(|e| format!("publisher connect: {e}"))?;
        Ok(Self {
            net,
            next_op: 0,
            pending: HashMap::new(),
            accepted: 0,
            done: 0,
            failed: Vec::new(),
            reconnects: 0,
        })
    }

    pub fn connected(&self) -> bool {
        self.net.status.borrow().state == ConnState::Connected
    }

    /// Counts the results that are in the event log. Call it often: `Net::send` fails with
    /// `Full` when the log is nearly full.
    pub fn drain(&mut self) {
        while let Ok(event) = self.net.events.try_recv() {
            match event {
                Event::Published { op_id, result, .. } => {
                    let expected = match self.pending.remove(&op_id) {
                        Some(Qos::AtMostOnce) => PublishResult::Sent,
                        Some(_) => PublishResult::Acknowledged,
                        None => {
                            self.failed.push(result);
                            continue;
                        }
                    };
                    if result == expected {
                        self.done += 1;
                    } else {
                        self.failed.push(result);
                    }
                }
                Event::Reconnected { .. } => self.reconnects += 1,
                _ => {}
            }
        }
    }

    /// Sends one message. It waits while the command queue or the event log is full.
    pub async fn send(&mut self, topic: &str, payload: Bytes, qos: Qos) -> Result<(), SendError> {
        loop {
            self.drain();
            self.next_op += 1;
            let cmd = Command::Publish(Box::new(Outgoing {
                op_id: self.next_op,
                topic: topic.into(),
                payload: payload.clone(),
                qos,
                retain: false,
                props: MsgProps::default(),
            }));
            match self.net.send(cmd) {
                Ok(()) => {
                    self.pending.insert(self.next_op, qos);
                    self.accepted += 1;
                    return Ok(());
                }
                Err(SendError::Full) => tokio::time::sleep(Duration::from_millis(1)).await,
                Err(SendError::Closed) => return Err(SendError::Closed),
            }
        }
    }

    /// Waits until every accepted message has a result, at most `limit`.
    pub async fn settle(&mut self, limit: Duration) {
        let end = tokio::time::Instant::now() + limit;
        loop {
            self.drain();
            if self.done + self.failed.len() as u64 >= self.accepted
                || tokio::time::Instant::now() >= end
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub async fn shutdown(self) {
        self.net.shutdown().await;
    }
}
