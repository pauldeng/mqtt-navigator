// LOCAL PATCH (mqtt-navigator). Not part of upstream ms-mqtt-client 0.1.0.
//
//! Process-wide limits for incoming data.
//!
//! Upstream reads the remaining length of a packet and reserves that much memory (up to 256 MiB)
//! without a check, and queues incoming publishes in an unbounded channel. These limits close both
//! gaps. They are global because mqtt-navigator runs one connection at a time.
// ponytail: global counters, one connection at a time. Move them into the client if the app
// ever needs more than one simultaneous connection.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static MAX_PACKET: AtomicUsize = AtomicUsize::new(usize::MAX);
static MAX_QUEUED: AtomicUsize = AtomicUsize::new(usize::MAX);
static MAX_QUEUED_BYTES: AtomicUsize = AtomicUsize::new(usize::MAX);
static QUEUED: AtomicUsize = AtomicUsize::new(0);
static QUEUED_BYTES: AtomicUsize = AtomicUsize::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Fixed cost added to each queued publish, for the bookkeeping.
pub(crate) const PUBLISH_OVERHEAD: usize = 128;
/// Fixed cost added to each user property, for its two string headers.
pub(crate) const USER_PROPERTY_OVERHEAD: usize = 48;

/// Sets the limits. `max_packet` is the largest incoming packet in bytes (fixed header included).
/// `max_queued` and `max_queued_bytes` bound the incoming-publish queue.
pub fn set_limits(max_packet: usize, max_queued: usize, max_queued_bytes: usize) {
    MAX_PACKET.store(max_packet, Ordering::Relaxed);
    MAX_QUEUED.store(max_queued, Ordering::Relaxed);
    MAX_QUEUED_BYTES.store(max_queued_bytes, Ordering::Relaxed);
}

/// Number of incoming publishes dropped because the queue was full.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Clears the queue counters. Call it before a new client starts.
pub fn reset_queue() {
    QUEUED.store(0, Ordering::Relaxed);
    QUEUED_BYTES.store(0, Ordering::Relaxed);
}

pub(crate) fn max_packet() -> usize {
    MAX_PACKET.load(Ordering::Relaxed)
}

/// Largest WebSocket frame or message that we accept. `None` means "no limit set": the
/// WebSocket library then keeps its own defaults.
pub(crate) fn ws_limit() -> Option<usize> {
    match max_packet() {
        usize::MAX => None,
        n => Some(n),
    }
}

/// Reserves queue space for one publish of `cost` bytes. Returns false (and counts a drop) if the
/// queue is full.
pub(crate) fn try_admit(cost: usize) -> bool {
    let count = QUEUED.fetch_add(1, Ordering::AcqRel) + 1;
    let bytes = QUEUED_BYTES.fetch_add(cost, Ordering::AcqRel) + cost;
    if count > MAX_QUEUED.load(Ordering::Relaxed)
        || bytes > MAX_QUEUED_BYTES.load(Ordering::Relaxed)
    {
        release(cost);
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

/// Frees queue space for one publish of `cost` bytes.
pub(crate) fn release(cost: usize) {
    QUEUED.fetch_sub(1, Ordering::AcqRel);
    QUEUED_BYTES.fetch_sub(cost, Ordering::AcqRel);
}
