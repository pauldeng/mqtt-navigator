//! Topic store: the live topic tree, per-topic history and the message list.
//!
//! The UI owns one `TopicStore` and calls it on the UI thread, so every method must be fast.
//! See the TopicStore section of the plan and ADR 0003.
//!
//! # Data
//!
//! - **Tree:** an arena of nodes. Each node has the full topic (`Arc<str>`), counts, the latest
//!   message and a history ring.
//! - **Message list:** one bounded list of rows, oldest first inside the store. Rows are received
//!   messages, sent messages and "Reconnected" markers. Each row has a [`RowId`]. Ids rise by one
//!   for every row, are never reused (not even after `clear`) and rows leave only at the old end.
//!   Row `id` is therefore at index `id - oldest_id`.
//!
//! # Memory
//!
//! One budget covers all of it. Every holder (latest value, history entry, list row, snapshot)
//! records its own charge when it is added and gives back exactly that charge when it is removed.
//! The store never looks at reference counts. `Bytes` buffers are shared between holders, so the
//! budget is an upper bound of the real memory.
//!
//! The budget is a soft target. An insert never evicts: it only checks the hard ceiling
//! ([`TopicStore::hard_limit`], the budget plus a quarter) and drops the message above it.
//! [`TopicStore::work`] evicts in batches: when the used bytes are above the budget, it evicts
//! down to 90 % of the budget. The eviction order is: oldest history entries, oldest list rows,
//! oldest latest payloads. The Messages view has priority over history, because history grows
//! with the topic count.
//!
//! Node metadata has its own caps (`topic_cap`, `node_budget`). When a cap is 95 % full, `work`
//! removes idle leaf topics (least recently updated first) down to 90 %. A removed node frees
//! its slot. A [`NodeId`] has a generation, so an old id is never wrong: it gives `None`.
//!
//! # Views and work
//!
//! - The list view and the tree view have filters. The list filter is kept up to date on each
//!   insert. A filter change starts a rebuild. The rebuild runs in slices in [`TopicStore::work`].
//!   While it runs, the list shows the newest matches first (the rebuild scans from new to old).
//! - [`TopicStore::set_paused`] freezes both views. Ingest continues inside the budget. The
//!   snapshot of the tree is built in slices by `work`.

use std::collections::btree_map::Values;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Bound;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;

use crate::model::{Incoming, MsgProps, Qos};
use crate::topic;

/// Items one step of node eviction handles. A node can hold a whole history ring, so this is
/// smaller than `WORK_CHUNK`.
const NODE_CHUNK: usize = 16;

/// Most topic levels. A deeper topic is dropped (counted as a topic drop).
pub const MAX_TOPIC_LEVELS: usize = 64;

/// The text filter looks at this many bytes of the payload.
pub const TEXT_FILTER_PAYLOAD_BYTES: usize = 1024;

/// Most rows in the frozen tree view (paused). Rows after this are not shown while paused.
pub const SNAPSHOT_MAX_ROWS: usize = 10_000;

/// Bytes of the value preview in the frozen tree view.
const SNAPSHOT_PREVIEW_BYTES: usize = 64;

/// While paused, the list may grow to this many times `list_len`, so the frozen rows stay.
const PAUSED_LIST_FACTOR: usize = 10;

/// Items one step of `work` handles before it looks at the clock. `work` always does one step.
const WORK_CHUNK: usize = 256;

/// Most rows an insert removes because the list is above its cap (after a pause).
const MAX_TRIM_PER_INSERT: usize = 8;

/// Longest text of `SendStatus::Rejected` that the store keeps. A sent row reserves this.
const STATUS_MAX: usize = 256;

/// Identity of a node: a slot in the arena and the generation of the slot. The store reuses the
/// slot of a removed node, but with a new generation. So an old `NodeId` never points to another
/// topic: [`TopicStore::get`] and the other lookups give `None` (or an empty result) for it, after
/// the node was removed (memory pressure, see [`TopicStore::work`]) or after `clear`.
/// [`TopicStore::node`] panics for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId {
    idx: u32,
    generation: u32,
}

impl NodeId {
    fn ix(self) -> usize {
        self.idx as usize
    }

    /// An id that no store has issued yet, for tests of stale ids.
    #[cfg(test)]
    fn raw(idx: u32) -> Self {
        Self { idx, generation: 0 }
    }
}

/// Identity of one row of the message list. Never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RowId(pub u64);

/// Default memory budget of the store: 128 MiB.
pub const BUDGET_DEFAULT: usize = 128 * 1024 * 1024;
/// Smallest budget that [`Limits::with_budget`] and [`TopicStore::set_budget`] accept: 32 MiB.
pub const BUDGET_MIN: usize = 32 * 1024 * 1024;
/// Largest budget that [`Limits::with_budget`] and [`TopicStore::set_budget`] accept: 2 GiB.
pub const BUDGET_MAX: usize = 2 * 1024 * 1024 * 1024;

/// Store limits. All values are in bytes except `topic_cap`, `history_len` and `list_len`.
/// A zero value becomes the default value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest stored latest-value payload. A longer payload is truncated (copied).
    pub latest_payload_max: usize,
    /// Most topic nodes (internal and leaf).
    pub topic_cap: usize,
    /// Budget for all stored data (latest values, history, list, properties, node metadata).
    pub budget: usize,
    /// Budget for node metadata only. Must not be more than `budget`.
    pub node_budget: usize,
    /// Most history messages for each topic (the newest message is one of them).
    pub history_len: usize,
    /// Largest payload in a history entry. A longer payload is truncated (copied).
    pub history_payload_max: usize,
    /// Most rows in the message list.
    pub list_len: usize,
}

impl Limits {
    /// The default limits with another budget. The budget is clamped to
    /// [`BUDGET_MIN`]..=[`BUDGET_MAX`]. The node budget is a quarter of it.
    pub fn with_budget(bytes: usize) -> Self {
        let budget = bytes.clamp(BUDGET_MIN, BUDGET_MAX);
        Self {
            latest_payload_max: 1024 * 1024,
            topic_cap: 200_000,
            budget,
            node_budget: budget / 4,
            history_len: 100,
            history_payload_max: 64 * 1024,
            list_len: 10_000,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::with_budget(BUDGET_DEFAULT)
    }
}

/// Stored copy of a message.
#[derive(Clone, Debug)]
pub struct Msg {
    /// Stored payload. It is the full payload unless `truncated` is true.
    pub payload: Bytes,
    pub truncated: bool,
    /// Size of the payload on the wire.
    pub original_size: usize,
    pub qos: Qos,
    pub retain: bool,
    pub props: MsgProps,
    pub received_at: SystemTime,
}

/// What `insert` and `record_sent` did with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    /// A received message is in the tree, the history and the list.
    Stored(NodeId),
    /// A sent message is in the list only (`record_sent`).
    Recorded(RowId),
    /// The topic is new and the topic cap or the node budget is full.
    TopicDropped,
    /// The message does not fit in the budget.
    Dropped,
}

/// Whether a list row is a received or a sent message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Received,
    Sent,
}

/// The "All | Received | Sent" filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectionFilter {
    #[default]
    All,
    Received,
    Sent,
}

/// Progress of a sent message. `Rejected` text is cut to 256 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendStatus {
    Sending,
    Sent,
    Acknowledged,
    Rejected(String),
    TimedOut,
    LostOnDisconnect,
}

/// Filter of the message list. The default filter shows everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFilter {
    /// Case-insensitive text. It must be in the topic, or in the first 1 KiB of the payload
    /// (decoded lossily). Empty means no text filter. Case folding is Unicode lowercase of the
    /// text and of the topic or payload.
    pub text: String,
    pub direction: DirectionFilter,
    /// A subscription filter. Only topics that match it (MQTT 5 rules) show.
    pub show_only: Option<String>,
}

/// Filter of the tree. The default filter shows everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeFilter {
    /// Case-insensitive text in the full topic path. Empty means no text filter.
    pub text: String,
    /// A subscription filter. Only topics that match it (MQTT 5 rules) show.
    pub show_only: Option<String>,
}

/// Read-only view of one list row.
#[derive(Clone, Copy, Debug)]
pub enum RowView<'a> {
    Message {
        id: RowId,
        /// The tree node of the topic. `None` for a sent message whose topic has no node.
        node: Option<NodeId>,
        topic: &'a str,
        msg: &'a Msg,
        direction: Direction,
        /// `Some` for a sent message.
        status: Option<&'a SendStatus>,
    },
    /// A marker row: the session connected again.
    Reconnected { id: RowId, at: SystemTime },
    /// Paused only: this row was in the frozen list, and the store removed it.
    Removed { id: RowId },
    /// Paused only: one row, above the `Removed` rows. Text: "Older messages were removed
    /// (memory limit)".
    RemovedMarker,
}

/// Read-only view of one node, for rendering one row.
#[derive(Clone, Copy, Debug)]
pub struct NodeView<'a> {
    pub id: NodeId,
    /// Last topic level, for example `temp` in `factory/line1/temp`.
    pub name: &'a str,
    /// Full topic, for example `factory/line1/temp`.
    pub topic: &'a str,
    pub depth: u16,
    /// With a tree filter: the node has visible children.
    pub has_children: bool,
    /// With a tree filter: the node shows its visible children (the filter opens them).
    pub expanded: bool,
    /// Messages received on this exact topic. Paused: the value at the time of the snapshot.
    pub msg_count: u64,
    /// Messages received on this topic and all topics below it. Paused: as `msg_count`.
    pub subtree_msgs: u64,
    /// Topics with a message below this node (not counting this node). Paused: as `msg_count`.
    pub subtree_topics: u32,
    /// The latest message. It is always live.
    pub latest: Option<&'a Msg>,
    /// Paused: the start (64 bytes, lossy UTF-8) of the latest payload at the time of the
    /// snapshot. `None` when the view is live, or when the node is not in the snapshot.
    pub preview: Option<&'a str>,
}

/// Counters for the status bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    pub topics: usize,
    /// Received messages that the store accepted.
    pub messages: u64,
    pub used_bytes: usize,
    pub topic_drops: u64,
    pub drops: u64,
    /// Bytes charged for history entries.
    pub history_bytes: usize,
    /// Rows in the message list (all rows, also the rows that a filter hides).
    pub list_rows: usize,
    /// Bytes charged for list rows.
    pub list_bytes: usize,
    /// List rows that the store removed because of the memory budget. The `list_len` cap does
    /// not count.
    pub list_drops: u64,
}

/// One history entry. `charge` is the amount that the store added to `hist_bytes`.
struct HistEntry {
    seq: u64,
    charge: usize,
    msg: Msg,
}

/// One node of the arena. Internal nodes and leaves look the same.
struct Node {
    name: Box<str>,
    /// Full topic. A leaf shares the `Arc` of the first message.
    topic: Arc<str>,
    parent: Option<NodeId>,
    depth: u16,
    /// Sorted by byte order of the name: `""` first, then `A`..`Z`, then `a`..`z`.
    children: BTreeMap<Box<str>, NodeId>,
    expanded: bool,
    msg_count: u64,
    subtree_msgs: u64,
    subtree_topics: u32,
    latest: Option<Msg>,
    /// Key of this node in `TopicStore::order`. Valid only while `latest` has a cost above 0.
    seq: u64,
    /// Newest last.
    history: VecDeque<HistEntry>,
    /// Colour cache: the subscription revision, and the index (`NO_COLOUR` for none).
    colour_rev: u32,
    colour_idx: u32,
    /// The broker keeps a retained message for this topic (see [`TopicStore::has_retained`]).
    retained: Option<Retained>,
    /// Generation of the slot. Rises each time the slot is freed.
    generation: u32,
    /// False for a free slot.
    live: bool,
    /// Receive sequence of the last message. It is the key in `TopicStore::touch` while the node
    /// is a leaf. 0: no message yet.
    touch: u64,
}

/// A retained message that the broker is known to keep.
#[derive(Clone, Copy)]
struct Retained {
    /// The Message Expiry Interval of the retained message ends here. `None`: it has none.
    until: Option<SystemTime>,
}

const NO_COLOUR: u32 = u32::MAX;
const ARC_OVERHEAD: usize = 16;

/// Estimated bytes of one node besides its name and topic: the struct, one entry in the parent
/// map, and one byte for the tree filter flags. The name is stored twice (node and parent map
/// key). The topic is stored once, with its `Arc` header.
const NODE_OVERHEAD: usize = size_of::<Node>() + 48 + 1;

fn node_cost(name_len: usize, topic_len: usize) -> usize {
    NODE_OVERHEAD + 2 * name_len + topic_len + ARC_OVERHEAD
}

/// Bytes that a latest value costs. The node struct already holds the fixed part.
fn msg_cost(msg: &Msg) -> usize {
    msg.payload.len() + msg.props.heap_bytes()
}

/// Fixed part of the charge of a history entry. The `VecDeque` can have spare room, so the
/// struct counts twice. The extra bytes are the entry in `hist_order`.
const HIST_OVERHEAD: usize = 2 * size_of::<HistEntry>() + 48;

/// Most retained publishes that wait for their end (see `TopicStore::pending_retained`).
const MAX_PENDING_RETAINED: usize = 64;
/// Most topics in `TopicStore::sent_retained`.
const MAX_SENT_RETAINED: usize = 1024;

/// Topics with our own acknowledged retained publish. A map for the lookup, and a second map for
/// the order, so the oldest topic goes first when the cap is full.
#[derive(Default)]
struct SentRetained {
    by_topic: HashMap<Arc<str>, (u64, Retained)>,
    /// Insert counter to topic. The first entry is the oldest.
    order: BTreeMap<u64, Arc<str>>,
    counter: u64,
}

impl SentRetained {
    fn remove(&mut self, topic: &str) {
        if let Some((n, _)) = self.by_topic.remove(topic) {
            self.order.remove(&n);
        }
    }

    fn insert(&mut self, topic: Arc<str>, r: Retained) {
        self.remove(&topic);
        if self.by_topic.len() >= MAX_SENT_RETAINED
            && let Some((_, old)) = self.order.pop_first()
        {
            self.by_topic.remove(&old);
        }
        self.counter += 1;
        self.order.insert(self.counter, Arc::clone(&topic));
        self.by_topic.insert(topic, (self.counter, r));
    }

    fn get(&self, topic: &str) -> Option<Retained> {
        self.by_topic.get(topic).map(|&(_, r)| r)
    }
}

struct PendingRetained {
    op_id: u64,
    topic: Arc<str>,
    qos: Qos,
    /// An empty payload: the publish deletes the retained message.
    delete: bool,
    until: Option<SystemTime>,
}

/// Fixed part of the charge of a list row. The extra bytes are the entry in the filter index.
const ROW_OVERHEAD: usize = size_of::<Row>() + 16;

/// Extra charge of a sent row: the entry in the `op_id` map and the room for the status text.
const SENT_EXTRA: usize = 48 + STATUS_MAX;

/// One row of the message list.
struct Row {
    id: RowId,
    /// The amount that the store added to `list_bytes`.
    charge: usize,
    kind: RowKind,
}

// ponytail: a marker row wastes about 250 bytes (it is charged the same). Box `MsgRow` if markers
// ever become frequent.
#[allow(clippy::large_enum_variant)]
enum RowKind {
    Message(MsgRow),
    Reconnected(SystemTime),
}

struct MsgRow {
    node: Option<NodeId>,
    /// The `Arc` of the node, or an own copy for a sent message without a node.
    topic: Arc<str>,
    msg: Msg,
    direction: Direction,
    status: Option<SendStatus>,
    /// The `op_id` of a sent message.
    op: Option<u64>,
}

impl Row {
    fn view(&self) -> RowView<'_> {
        match &self.kind {
            RowKind::Message(m) => RowView::Message {
                id: self.id,
                node: m.node,
                topic: &m.topic,
                msg: &m.msg,
                direction: m.direction,
                status: m.status.as_ref(),
            },
            RowKind::Reconnected(at) => RowView::Reconnected {
                id: self.id,
                at: *at,
            },
        }
    }
}

/// The subscription filters, parsed once and indexed by their first level.
///
/// A lookup collects the filters whose first level is the first level of the topic, and the
/// filters that start with a wildcard. The first filter in list order that matches wins. The cost
/// is O(topic depth + candidates), not O(all filters).
#[derive(Default)]
struct SubIndex {
    /// The list as the caller gave it. It is used to see that a new list is the same.
    raw: Vec<String>,
    /// Parsed filters. `None`: not valid, so it matches nothing.
    parsed: Vec<Option<topic::Filter>>,
    /// First literal level to the indexes (ascending) of the filters that start with it.
    by_first: HashMap<Box<str>, Vec<u32>>,
    /// Indexes (ascending) of the filters that start with `+` or `#`.
    wild: Vec<u32>,
}

impl SubIndex {
    fn new(raw: Vec<String>) -> Self {
        let mut by_first: HashMap<Box<str>, Vec<u32>> = HashMap::new();
        let mut wild = Vec::new();
        let parsed = raw
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let f = topic::Filter::new(s)?;
                let i = u32::try_from(i).ok()?;
                if f.starts_with_wildcard() {
                    wild.push(i);
                } else {
                    by_first.entry(f.first_level().into()).or_default().push(i);
                }
                Some(f)
            })
            .collect();
        Self {
            raw,
            parsed,
            by_first,
            wild,
        }
    }

    /// Index of the first filter that matches `topic`.
    fn first_match(&self, topic: &str) -> Option<usize> {
        let first = topic.split('/').next().unwrap_or_default();
        let hit = |list: &[u32]| {
            list.iter().copied().find(|&i| {
                self.parsed[i as usize]
                    .as_ref()
                    .is_some_and(|f| f.matches(topic))
            })
        };
        let lit = self.by_first.get(first).and_then(|l| hit(l));
        let wild = hit(&self.wild);
        lit.into_iter().chain(wild).min().map(|i| i as usize)
    }
}

/// A case-insensitive text to look for.
#[derive(Default)]
struct Needle {
    lc: String,
    ascii: bool,
}

impl Needle {
    fn new(text: &str) -> Self {
        let ascii = text.is_ascii();
        let lc = if ascii {
            text.to_ascii_lowercase()
        } else {
            text.to_lowercase()
        };
        Self { lc, ascii }
    }

    /// An empty needle is in every text. The case rule is Unicode lowercase of both sides. The
    /// ASCII fast path gives the same result: it needs an ASCII needle and an ASCII text (for
    /// example the Kelvin sign U+212A lowercases to `k`).
    fn found_in(&self, hay: &str) -> bool {
        if self.lc.is_empty() {
            return true;
        }
        if self.ascii && hay.is_ascii() {
            // No allocation: this runs for each arriving message when a text filter is set.
            let n = self.lc.as_bytes();
            hay.as_bytes()
                .windows(n.len())
                .any(|w| w.eq_ignore_ascii_case(n))
        } else {
            // ponytail: allocates for a non-ASCII text. Add a char-wise search if it shows up.
            hay.to_lowercase().contains(&self.lc)
        }
    }
}

/// The list filter and the data that the matching needs.
#[derive(Default)]
struct ListFilterState {
    filter: ListFilter,
    needle: Needle,
    /// False for the default filter. Then the list itself is the view and there is no index.
    active: bool,
    /// `filter.show_only`, parsed once. `None` for a filter that is not valid.
    show: Option<topic::Filter>,
}

impl ListFilterState {
    fn new(filter: ListFilter) -> Self {
        Self {
            needle: Needle::new(&filter.text),
            show: filter.show_only.as_deref().and_then(topic::Filter::new),
            active: !filter.text.is_empty()
                || filter.direction != DirectionFilter::All
                || filter.show_only.is_some(),
            filter,
        }
    }
}

/// A marker row always matches. It separates sessions.
fn row_matches(row: &Row, f: &ListFilterState) -> bool {
    let RowKind::Message(m) = &row.kind else {
        return true;
    };
    match f.filter.direction {
        DirectionFilter::All => {}
        DirectionFilter::Received if m.direction == Direction::Received => {}
        DirectionFilter::Sent if m.direction == Direction::Sent => {}
        _ => return false,
    }
    if f.filter.show_only.is_some() && !f.show.as_ref().is_some_and(|s| s.matches(&m.topic)) {
        return false;
    }
    f.needle.found_in(&m.topic) || {
        let p = &m.msg.payload;
        let head = &p[..p.len().min(TEXT_FILTER_PAYLOAD_BYTES)];
        f.needle.found_in(&String::from_utf8_lossy(head))
    }
}

#[derive(Default)]
struct TreeFilterState {
    filter: TreeFilter,
    needle: Needle,
    active: bool,
    /// `filter.show_only`, parsed once. `None` for a filter that is not valid.
    show: Option<topic::Filter>,
}

/// Tree flags of a node, with a tree filter.
const T_MATCH: u8 = 1;
const T_VISIBLE: u8 = 2;
/// The node has a match below it. The filter shows such a node open.
const T_OPEN: u8 = 4;

/// What the tree shows while paused, for one node.
struct SnapEntry {
    msg_count: u64,
    subtree_msgs: u64,
    subtree_topics: u32,
    preview: Box<str>,
}

/// Charge of one entry of the frozen tree: the row id, the map entry and the entry itself.
const SNAP_OVERHEAD: usize = size_of::<SnapEntry>() + 3 * size_of::<NodeId>() + 32;

/// A resumable walk over the tree (depth first, children a-z) that builds the frozen tree view.
struct SnapJob {
    /// One frame for each open level: the parent and the last child that the walk handled.
    /// A child is found again by its name, so new nodes can arrive between two steps.
    stack: Vec<SnapFrame>,
    /// Nodes with an index at or above this one were created after the pause. The snapshot
    /// does not show them.
    node_limit: usize,
}

struct SnapFrame {
    parent: Option<NodeId>,
    last: Option<NodeId>,
}

pub struct TopicStore {
    limits: Limits,
    /// Arena of slots. A `NodeId` is a slot and its generation. A free slot has `live == false`.
    nodes: Vec<Node>,
    /// Free slots (indexes). Not used while paused (see `alloc`).
    free: Vec<u32>,
    /// Nodes that are not free.
    live_nodes: usize,
    /// The leaves (nodes without children), oldest message first. Key is `Node::touch`.
    /// These are the nodes that memory pressure can remove.
    touch: BTreeMap<u64, NodeId>,
    /// Shared empty topic for the free slots.
    empty: Arc<str>,
    /// A batch eviction of data runs (from above the budget to the low watermark).
    evicting: bool,
    /// A batch eviction of nodes runs.
    evicting_nodes: bool,
    /// The last batch found nothing to evict. Cleared by the next insert.
    stuck_mem: bool,
    stuck_nodes: bool,
    /// Children of the virtual root. Topics start at depth 0.
    roots: BTreeMap<Box<str>, NodeId>,
    /// Visible nodes in display order. Valid only when `rows_dirty` is false.
    rows: Vec<NodeId>,
    rows_dirty: bool,
    /// Nodes with a non-empty latest value, oldest first. Key is the receive sequence.
    order: BTreeMap<u64, NodeId>,
    /// History entries of all nodes, oldest first. The oldest entry is the front of a ring.
    hist_order: BTreeMap<u64, NodeId>,
    seq: u64,
    /// Bytes of node metadata.
    node_bytes: usize,
    /// Bytes of all latest values (payload and properties).
    latest_bytes: usize,
    hist_bytes: usize,
    list_bytes: usize,
    /// Bytes of the frozen tree view.
    snap_bytes: usize,
    messages: u64,
    topic_drops: u64,
    drops: u64,
    list_drops: u64,

    // Message list.
    /// Oldest first. Ids are consecutive.
    list: VecDeque<Row>,
    /// Id of the next row. It is not reset by `clear`.
    next_row: u64,
    /// `op_id` to the row of a sent message.
    ops: HashMap<u64, RowId>,
    /// Our own retained publishes that wait for the end of the send. They do not depend on a
    /// list row, which the store can evict. Oldest first.
    pending_retained: VecDeque<PendingRetained>,
    /// Topics where our own retained publish succeeded, also topics without a node (we do not
    /// subscribe to them). Oldest first.
    sent_retained: SentRetained,
    lf: ListFilterState,
    /// With an active filter: the ids of the matching rows, ascending.
    matched: VecDeque<RowId>,
    /// A rebuild scans the rows with an id below `scan_below` (and not below the oldest row).
    scanning: bool,
    scan_below: u64,
    filter_gen: u64,

    // Tree filter.
    tf: TreeFilterState,
    /// One flag byte for each node, only while the tree filter is active.
    tree_flags: Vec<u8>,
    /// Nodes with an index below this one have their flags.
    tree_cursor: usize,

    // Pause.
    paused: bool,
    /// Paused: ids below this one are in the frozen list.
    cutoff: u64,
    paused_new: u64,
    /// Paused: ids (ascending) of frozen rows that the store removed.
    removed: Vec<RowId>,
    snap_rows: Vec<NodeId>,
    snap: HashMap<NodeId, SnapEntry>,
    /// Paused: the snapshot is being built (see `work`). `None` when it is complete.
    snap_job: Option<SnapJob>,

    // Subscription colours.
    subs: SubIndex,
    sub_rev: u32,
}

impl TopicStore {
    /// Creates a store. Bad limits are fixed, not rejected: a zero value becomes the default
    /// value, `topic_cap` is at most `u32::MAX` (ids are `u32`), and `node_budget` is at most
    /// `budget`.
    pub fn new(limits: Limits) -> Self {
        let d = Limits::default();
        let or_default = |v: usize, default: usize| if v == 0 { default } else { v };
        let budget = or_default(limits.budget, d.budget);
        let limits = Limits {
            latest_payload_max: or_default(limits.latest_payload_max, d.latest_payload_max),
            topic_cap: or_default(limits.topic_cap, d.topic_cap)
                .min(usize::try_from(u32::MAX).unwrap_or(usize::MAX)),
            budget,
            node_budget: or_default(limits.node_budget, d.node_budget).min(budget),
            history_len: or_default(limits.history_len, d.history_len),
            history_payload_max: or_default(limits.history_payload_max, d.history_payload_max),
            list_len: or_default(limits.list_len, d.list_len),
        };
        Self {
            limits,
            nodes: Vec::new(),
            free: Vec::new(),
            live_nodes: 0,
            touch: BTreeMap::new(),
            empty: Arc::from(""),
            evicting: false,
            evicting_nodes: false,
            stuck_mem: false,
            stuck_nodes: false,
            roots: BTreeMap::new(),
            rows: Vec::new(),
            rows_dirty: false,
            order: BTreeMap::new(),
            hist_order: BTreeMap::new(),
            seq: 0,
            node_bytes: 0,
            latest_bytes: 0,
            hist_bytes: 0,
            list_bytes: 0,
            snap_bytes: 0,
            messages: 0,
            topic_drops: 0,
            pending_retained: VecDeque::new(),
            sent_retained: SentRetained::default(),
            drops: 0,
            list_drops: 0,
            list: VecDeque::new(),
            next_row: 0,
            ops: HashMap::new(),
            lf: ListFilterState::default(),
            matched: VecDeque::new(),
            scanning: false,
            scan_below: 0,
            filter_gen: 0,
            tf: TreeFilterState::default(),
            tree_flags: Vec::new(),
            tree_cursor: 0,
            paused: false,
            cutoff: 0,
            paused_new: 0,
            removed: Vec::new(),
            snap_rows: Vec::new(),
            snap: HashMap::new(),
            snap_job: None,
            subs: SubIndex::default(),
            sub_rev: 1,
        }
    }

    /// Removes all nodes, messages and rows. Counters reset too.
    ///
    /// These stay: the limits, the filters, the subscriptions and the next `RowId` (an old
    /// `RowId` never points to a new row). Pause ends.
    pub fn clear(&mut self) {
        drop(self.take());
    }

    /// Like [`clear`](Self::clear), but returns the old store. Dropping a big store takes a long
    /// time, so the caller can drop the returned store on a background thread. The returned
    /// store keeps the data and the counters. Its filters and subscriptions are moved to the new
    /// store, so it is only good for reading or for a drop.
    pub fn take(&mut self) -> TopicStore {
        let mut fresh = Self::new(self.limits);
        fresh.next_row = self.next_row;
        fresh.scan_below = self.next_row;
        fresh.filter_gen = self.filter_gen + 1;
        fresh.lf = std::mem::take(&mut self.lf);
        fresh.tf = std::mem::take(&mut self.tf);
        fresh.subs = std::mem::take(&mut self.subs);
        fresh.sub_rev = self.sub_rev.wrapping_add(1).max(1);
        std::mem::replace(self, fresh)
    }

    // ---------------------------------------------------------------------------------------
    // Insert
    // ---------------------------------------------------------------------------------------

    /// Inserts one received message. Creates missing nodes. Updates counts on the path. Adds the
    /// message to the latest value, the history of the topic and the message list.
    /// Never goes above the hard ceiling and the node caps. `msg.generation` is not checked here.
    ///
    /// The store checks the whole cost first. It never evicts here: it only drops the message if
    /// the memory would go above the hard ceiling (`hard_limit`). [`TopicStore::work`] evicts in
    /// batches, so the memory can be a little above the budget for a short time.
    /// The topic keeps its old latest value if the message is dropped.
    pub fn insert(&mut self, msg: Incoming) -> Insert {
        let Incoming {
            topic,
            payload,
            qos,
            retain,
            props,
            received_at,
            ..
        } = msg;

        // A very deep topic would create many nodes in one insert and break the UI frame budget.
        if topic.bytes().filter(|&b| b == b'/').count() >= MAX_TOPIC_LEVELS {
            self.topic_drops += 1;
            return Insert::TopicDropped;
        }

        // Walk the existing part of the path. No allocation.
        let mut parent = None;
        let mut parts = topic.split('/');
        let mut first_missing = None;
        for part in parts.by_ref() {
            match self.children_of(parent).get(part) {
                Some(&id) => parent = Some(id),
                None => {
                    first_missing = Some(part);
                    break;
                }
            }
        }
        let missing = first_missing.into_iter().chain(parts);
        let (new_nodes, new_node_bytes) = missing.clone().fold((0, 0), |(n, b), part| {
            (n + 1, b + node_cost(part.len(), prefix_end(&topic, part)))
        });

        // Admission: compute the full cost before any change.
        let original_size = payload.len();
        let stored_len = original_size.min(self.limits.latest_payload_max);
        let props_cost = props.heap_bytes();
        let expiry = props.message_expiry;
        let latest_cost = stored_len + props_cost;
        let hist_len = stored_len.min(self.limits.history_payload_max);
        let hist_cost = HIST_OVERHEAD + hist_len + props_cost;
        let row_cost = ROW_OVERHEAD + stored_len + props_cost;
        let cost = latest_cost + hist_cost + row_cost;
        if new_nodes > 0
            && (self.live_nodes + new_nodes > self.limits.topic_cap
                || self.node_bytes + new_node_bytes > self.limits.node_budget)
        {
            self.topic_drops += 1;
            return Insert::TopicDropped;
        }
        // The old latest value of this topic is replaced at once, so it counts as free space.
        let existing = if new_nodes == 0 { parent } else { None };
        let own = existing.map_or(0, |id| {
            self.nodes[id.ix()].latest.as_ref().map_or(0, msg_cost)
        });
        if self.used() + new_node_bytes + cost > self.hard_limit() + own {
            self.drops += 1;
            return Insert::Dropped;
        }
        self.stuck_mem = false;
        self.stuck_nodes = false;
        if let Some(id) = existing {
            self.release(id);
        }
        debug_assert!(self.used() + new_node_bytes + cost <= self.hard_limit());

        // Create the missing nodes.
        let mut leaf = parent;
        for (i, name) in missing.enumerate() {
            let depth = leaf.map_or(0, |p| self.nodes[p.ix()].depth.saturating_add(1));
            let end = prefix_end(&topic, name);
            let node_topic = if end == topic.len() {
                Arc::clone(&topic)
            } else {
                Arc::from(&topic[..end])
            };
            let id = self.alloc(Node {
                name: name.into(),
                topic: node_topic,
                parent: leaf,
                depth,
                children: BTreeMap::new(),
                expanded: false,
                msg_count: 0,
                subtree_msgs: 0,
                subtree_topics: 0,
                latest: None,
                seq: 0,
                history: VecDeque::new(),
                colour_rev: 0,
                colour_idx: NO_COLOUR,
                retained: None,
                generation: 0,
                live: true,
                touch: 0,
            });
            self.node_bytes += node_cost(name.len(), end);
            match leaf {
                None => self.roots.insert(name.into(), id),
                Some(p) => {
                    // The parent gets its first child: it is not a leaf any more.
                    let pn = &mut self.nodes[p.ix()];
                    if pn.children.is_empty() {
                        self.touch.remove(&pn.touch);
                    }
                    pn.children.insert(name.into(), id)
                }
            };
            // Only the first new node can show up in the rows: the others are below a new
            // (collapsed) node. With a tree filter, `tree_test` decides.
            if i == 0 && !self.tf.active && !self.rows_dirty && self.children_shown(parent) {
                self.splice_new_row(id);
            }
            if self.tf.active {
                match self.tree_flags.get_mut(id.ix()) {
                    Some(f) => *f = 0, // a reused slot
                    None => self.tree_flags.push(0),
                }
                // A node above the cursor is tested by the rebuild. A node at the cursor or below
                // it (a reused slot) is tested now.
                if id.ix() <= self.tree_cursor {
                    self.tree_test(id);
                    if self.tree_cursor == id.ix() {
                        self.tree_cursor += 1;
                    }
                }
            }
            leaf = Some(id);
        }
        let Some(id) = leaf else {
            // A topic has at least one level, so this cannot happen.
            return Insert::Dropped;
        };

        // Build the stored copies. A truncated payload is copied so it does not keep the big
        // buffer. Otherwise all copies share one buffer.
        let truncated = stored_len < original_size;
        let payload = if truncated {
            Bytes::copy_from_slice(&payload[..stored_len])
        } else {
            payload
        };
        let latest = Msg {
            payload,
            truncated,
            original_size,
            qos,
            retain,
            props,
            received_at,
        };
        let hist_msg = if latest.payload.len() > hist_len {
            Msg {
                payload: Bytes::copy_from_slice(&latest.payload[..hist_len]),
                truncated: true,
                ..latest.clone()
            }
        } else {
            latest.clone()
        };
        let row_msg = latest.clone();

        // Store the latest value.
        // Any retained message from the broker is the newest record for its topic: the node keeps
        // it, and our own earlier retained publish no longer counts.
        if retain && !self.sent_retained.by_topic.is_empty() {
            self.sent_retained.remove(&topic);
        }
        let node = &mut self.nodes[id.ix()];
        let first = node.msg_count == 0;
        node.msg_count += 1;
        // A retained message with a payload sets the broker state. A retained message with an
        // empty payload deletes it. A message without the flag changes nothing.
        if retain {
            node.retained = (original_size > 0).then(|| Retained {
                until: expiry
                    .and_then(|s| received_at.checked_add(Duration::from_secs(u64::from(s)))),
            });
        }
        node.latest = Some(latest);
        let node_topic = Arc::clone(&node.topic);
        if latest_cost > 0 {
            self.seq += 1;
            node.seq = self.seq;
            self.order.insert(self.seq, id);
            self.latest_bytes += latest_cost;
        }
        self.messages += 1;

        // Store the history entry. The ring has a cap. The oldest entry of a full ring goes.
        self.seq += 1;
        node.history.push_back(HistEntry {
            seq: self.seq,
            charge: hist_cost,
            msg: hist_msg,
        });
        self.hist_order.insert(self.seq, id);
        self.hist_bytes += hist_cost;
        // A leaf is indexed by its last message, for the removal of idle topics.
        if node.children.is_empty() {
            self.touch.remove(&node.touch);
            self.touch.insert(self.seq, id);
        }
        node.touch = self.seq;
        while node.history.len() > self.limits.history_len {
            if let Some(old) = node.history.pop_front() {
                self.hist_order.remove(&old.seq);
                self.hist_bytes -= old.charge;
            }
        }

        // Counts: this node and all ancestors.
        self.nodes[id.ix()].subtree_msgs += 1;
        let mut up = self.nodes[id.ix()].parent;
        while let Some(p) = up {
            let n = &mut self.nodes[p.ix()];
            n.subtree_msgs += 1;
            if first {
                n.subtree_topics += 1;
            }
            up = n.parent;
        }

        // The list row.
        self.admit_row(
            RowKind::Message(MsgRow {
                node: Some(id),
                topic: node_topic,
                msg: row_msg,
                direction: Direction::Received,
                status: None,
                op: None,
            }),
            row_cost,
            true,
        );
        Insert::Stored(id)
    }

    /// Adds a sent message to the message list. It does not change the tree, the latest values
    /// or the history: the broker sends the message back as a normal received message.
    ///
    /// The row shows `SendStatus::Sending` until `set_send_status` changes it. The store keeps
    /// the `op_id` for as long as the row is in the list. Returns `Insert::Recorded` or
    /// `Insert::Dropped` (the row does not fit in the budget).
    #[allow(clippy::too_many_arguments)] // one call for one fact, a struct would only wrap them
    pub fn record_sent(
        &mut self,
        op_id: u64,
        topic: &str,
        payload: Bytes,
        qos: Qos,
        retain: bool,
        props: MsgProps,
        at: SystemTime,
    ) -> Insert {
        let node = self.find(topic);
        let shared = node.map(|n| Arc::clone(&self.nodes[n.ix()].topic));
        let topic_cost = if shared.is_some() {
            0
        } else {
            topic.len() + ARC_OVERHEAD
        };
        let original_size = payload.len();
        let expiry = props.message_expiry;
        let stored_len = original_size.min(self.limits.latest_payload_max);
        let cost = ROW_OVERHEAD + SENT_EXTRA + topic_cost + stored_len + props.heap_bytes();
        if self.used() + cost > self.hard_limit() {
            self.drops += 1;
            return Insert::Dropped;
        }
        let truncated = stored_len < original_size;
        let payload = if truncated {
            Bytes::copy_from_slice(&payload[..stored_len])
        } else {
            payload
        };
        let id = self.admit_row(
            RowKind::Message(MsgRow {
                node,
                topic: shared.unwrap_or_else(|| Arc::from(topic)),
                msg: Msg {
                    payload,
                    truncated,
                    original_size,
                    qos,
                    retain,
                    props,
                    received_at: at,
                },
                direction: Direction::Sent,
                status: Some(SendStatus::Sending),
                op: Some(op_id),
            }),
            cost,
            true,
        );
        // A repeated `op_id` points to the newest row.
        self.ops.insert(op_id, id);
        if retain {
            if self.pending_retained.len() >= MAX_PENDING_RETAINED {
                self.pending_retained.pop_front();
            }
            self.pending_retained.push_back(PendingRetained {
                op_id,
                topic: Arc::from(topic),
                qos,
                delete: original_size == 0,
                until: expiry.and_then(|s| at.checked_add(Duration::from_secs(u64::from(s)))),
            });
        }
        Insert::Recorded(id)
    }

    /// Changes the status of the sent message with this `op_id`. Returns false if the row is
    /// not in the list any more (or never was).
    pub fn set_send_status(&mut self, op_id: u64, status: SendStatus) -> bool {
        self.finish_retained(op_id, &status);
        let Some(&id) = self.ops.get(&op_id) else {
            return false;
        };
        let status = match status {
            SendStatus::Rejected(mut s) => {
                if s.len() > STATUS_MAX {
                    let mut n = STATUS_MAX;
                    while !s.is_char_boundary(n) {
                        n -= 1;
                    }
                    s.truncate(n);
                }
                // The row reserves STATUS_MAX bytes. A kept big buffer would be unaccounted.
                s.shrink_to_fit();
                SendStatus::Rejected(s)
            }
            other => other,
        };
        let front = self.front_id();
        let Some(row) =
            id.0.checked_sub(front)
                .and_then(|i| self.list.get_mut(usize::try_from(i).ok()?))
        else {
            return false;
        };
        let RowKind::Message(m) = &mut row.kind else {
            return false;
        };
        m.status = Some(status);
        true
    }

    /// Our own retained publish succeeds when the broker acknowledges it (QoS 1) or when it is
    /// written (QoS 0). A retained message with a payload is then held by the broker. An empty
    /// one deletes it. Any other end only forgets the publish. This needs no list row and no
    /// node.
    fn finish_retained(&mut self, op_id: u64, status: &SendStatus) {
        if matches!(status, SendStatus::Sending) {
            return;
        }
        let Some(i) = self.pending_retained.iter().position(|p| p.op_id == op_id) else {
            return;
        };
        let done = matches!(
            (status, self.pending_retained[i].qos),
            (SendStatus::Acknowledged, _) | (SendStatus::Sent, Qos::AtMostOnce)
        );
        // A QoS 1 publish is still open after "Sent": the acknowledgement comes later.
        if !done && matches!(status, SendStatus::Sent) {
            return;
        }
        let Some(p) = self.pending_retained.remove(i) else {
            return;
        };
        if !done {
            return;
        }
        let state = (!p.delete).then_some(Retained { until: p.until });
        if let Some(id) = self.find(&p.topic) {
            self.nodes[id.ix()].retained = state;
        }
        self.sent_retained.remove(&p.topic);
        if let Some(r) = state {
            self.sent_retained.insert(p.topic, r);
        }
    }

    /// [`has_retained`](Self::has_retained) for a topic path: the node state, or our own
    /// acknowledged retained publish to a topic that has no node.
    pub fn has_retained_topic(&self, topic: &str) -> bool {
        self.has_retained_topic_at(topic, SystemTime::now())
    }

    pub fn has_retained_topic_at(&self, topic: &str, now: SystemTime) -> bool {
        self.find(topic)
            .is_some_and(|id| self.has_retained_at(id, now))
            || self
                .sent_retained
                .get(topic)
                .is_some_and(|r| r.until.is_none_or(|u| now < u))
    }

    /// True if the broker keeps a retained message for this topic, as far as this client knows:
    /// a retained message with a payload arrived, and no retained delete (an empty retained
    /// message, or our own acknowledged "clear") came after it. A message without the retain flag
    /// does not change this. The Message Expiry Interval of the retained message ends it.
    pub fn has_retained(&self, id: NodeId) -> bool {
        self.has_retained_at(id, SystemTime::now())
    }

    /// [`has_retained`](Self::has_retained) at a given time. The expiry is computed on read:
    /// there is no timer.
    pub fn has_retained_at(&self, id: NodeId, now: SystemTime) -> bool {
        self.live(id)
            .and_then(|n| n.retained)
            .is_some_and(|r| r.until.is_none_or(|t| now < t))
    }

    /// Adds a "Reconnected" marker row to the list. The marker has a small fixed charge and it
    /// is not counted in `paused_new`. Returns `None` if even a marker does not fit.
    pub fn mark_reconnected(&mut self, at: SystemTime) -> Option<RowId> {
        if self.used() + ROW_OVERHEAD > self.hard_limit() {
            return None;
        }
        Some(self.admit_row(RowKind::Reconnected(at), ROW_OVERHEAD, false))
    }

    // ---------------------------------------------------------------------------------------
    // Tree view
    // ---------------------------------------------------------------------------------------

    /// Visible rows in display order (depth first, children a–z).
    ///
    /// - Live, no tree filter: only the rows below expanded nodes.
    /// - Live, with a tree filter: the matches and their ancestors. The filter opens the
    ///   ancestors, so `expanded` of the nodes does not matter.
    /// - Paused: the frozen rows (at most `SNAPSHOT_MAX_ROWS`). The snapshot is built in slices
    ///   by `work`. Until it is complete (`has_work`), this shows the live rows, as if not paused.
    ///
    /// Rebuilds the list only if the structure, the expansion or a filter changed.
    pub fn rows(&mut self) -> &[NodeId] {
        if self.paused && self.snap_job.is_none() {
            return &self.snap_rows;
        }
        self.live_rows()
    }

    fn live_rows(&mut self) -> &[NodeId] {
        if self.rows_dirty {
            // Only a tree filter (or a structure change that the splice cannot handle) gets
            // here. This is one walk for any number of changes since the last call.
            self.rows = self.rebuild_rows();
            self.rows_dirty = false;
        }
        &self.rows
    }

    /// The visible rows, built from the tree: O(visible rows).
    fn rebuild_rows(&self) -> Vec<NodeId> {
        let mut rows = Vec::with_capacity(self.rows.len());
        self.walk_rows(self.roots.values(), &mut rows);
        rows
    }

    /// Adds the visible rows below one list of siblings, depth first.
    fn walk_rows(&self, first: Values<'_, Box<str>, NodeId>, rows: &mut Vec<NodeId>) {
        let filtered = self.tf.active;
        let flag = |id: NodeId| self.tree_flags.get(id.ix()).copied().unwrap_or(0);
        let mut stack = vec![first];
        while let Some(it) = stack.last_mut() {
            match it.next() {
                Some(&id) => {
                    if filtered && flag(id) & T_VISIBLE == 0 {
                        continue;
                    }
                    rows.push(id);
                    let n = &self.nodes[id.ix()];
                    let open = if filtered {
                        flag(id) & T_OPEN != 0
                    } else {
                        n.expanded
                    };
                    if open {
                        stack.push(n.children.values());
                    }
                }
                None => {
                    stack.pop();
                }
            }
        }
    }

    /// True if the children of `parent` (`None`: the root) are on screen when `parent` is
    /// expanded: `parent` and all its ancestors are expanded. Without a tree filter only.
    fn children_shown(&self, parent: Option<NodeId>) -> bool {
        let mut cur = parent;
        while let Some(id) = cur {
            let n = &self.nodes[id.ix()];
            if !n.expanded {
                return false;
            }
            cur = n.parent;
        }
        true
    }

    /// Place of a node in `rows`, or the place where it would go. The rows are sorted by path,
    /// level by level (depth first, children a-z), so this is a binary search.
    fn row_place(&self, id: NodeId) -> Result<usize, usize> {
        let topic = &*self.nodes[id.ix()].topic;
        self.rows.binary_search_by(|r| {
            let t = &*self.nodes[r.ix()].topic;
            t.split('/').cmp(topic.split('/'))
        })
    }

    /// Puts a new node into `rows` (no tree filter, `rows` is not dirty, its parent shows its
    /// children). O(log V) to find the place, and one memmove.
    fn splice_new_row(&mut self, id: NodeId) {
        match self.row_place(id) {
            Err(at) => self.rows.insert(at, id),
            Ok(_) => self.rows_dirty = true, // cannot happen: the node is new
        }
    }

    /// Changes `rows` for an expand or a collapse (no tree filter, `rows` is not dirty).
    /// Costs the size of the changed part, and one memmove.
    fn splice_toggle(&mut self, id: NodeId) {
        let n = &self.nodes[id.ix()];
        if !self.children_shown(n.parent) {
            return; // the node is not on screen, so its rows do not change
        }
        let Ok(at) = self.row_place(id) else {
            self.rows_dirty = true; // cannot happen: a shown node has a row
            return;
        };
        if n.expanded {
            let mut below = Vec::new();
            self.walk_rows(n.children.values(), &mut below);
            self.rows.splice(at + 1..at + 1, below);
        } else {
            let depth = n.depth;
            let end = at
                + 1
                + self.rows[at + 1..]
                    .iter()
                    .take_while(|r| self.nodes[r.ix()].depth > depth)
                    .count();
            self.rows.drain(at + 1..end);
        }
    }

    /// View of one node. `id` must come from this store since the last `clear`, or this panics.
    /// Use [`TopicStore::get`] for an id that can be stale.
    pub fn node(&self, id: NodeId) -> NodeView<'_> {
        self.get(id).expect("NodeId from this store")
    }

    /// View of one node, or `None` if `id` is unknown (for example from before `clear`).
    /// While paused and with a complete snapshot, the counts and the preview come from the
    /// snapshot (if the node is in it).
    pub fn get(&self, id: NodeId) -> Option<NodeView<'_>> {
        let n = self.live(id)?;
        let snap = if self.paused && self.snap_job.is_none() {
            self.snap.get(&id)
        } else {
            None
        };
        let (has_children, expanded) = if self.tf.active {
            let open = self.tree_flags.get(id.ix()).copied().unwrap_or(0) & T_OPEN != 0;
            (open, open)
        } else {
            (!n.children.is_empty(), n.expanded)
        };
        Some(NodeView {
            id,
            name: &n.name,
            topic: &n.topic,
            depth: n.depth,
            has_children,
            expanded,
            msg_count: snap.map_or(n.msg_count, |s| s.msg_count),
            subtree_msgs: snap.map_or(n.subtree_msgs, |s| s.subtree_msgs),
            subtree_topics: snap.map_or(n.subtree_topics, |s| s.subtree_topics),
            latest: n.latest.as_ref(),
            preview: snap.map(|s| &*s.preview),
        })
    }

    /// Full topic path of a node, for example `factory/line1/temp`.
    /// An unknown id gives an empty string.
    pub fn path(&self, id: NodeId) -> String {
        self.live(id)
            .map_or_else(String::new, |n| n.topic.to_string())
    }

    /// Node of an exact topic path, if it exists.
    pub fn find(&self, topic: &str) -> Option<NodeId> {
        let mut cur = None;
        for part in topic.split('/') {
            cur = Some(*self.children_of(cur).get(part)?);
        }
        cur
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.live(id)?.parent
    }

    /// Expands or collapses a node. An unknown id is ignored.
    /// A tree filter ignores the flag until the filter is cleared. While paused, this starts a
    /// new snapshot (built by `work`), so the counts of the tree refresh once.
    pub fn set_expanded(&mut self, id: NodeId, expanded: bool) {
        if let Some(n) = self.live_mut(id)
            && n.expanded != expanded
        {
            n.expanded = expanded;
            // A tree filter shows its own set of rows, so the flag does not change them.
            if !self.tf.active && !self.rows_dirty {
                self.splice_toggle(id);
            }
            if self.paused {
                self.start_snapshot();
            }
        }
    }

    /// The newest messages of a topic, newest first (at most `history_len`).
    /// The store can evict history entries while the latest value stays, so this can be empty
    /// or shorter than `msg_count` when the memory budget is tight. The payload of an entry is
    /// at most `history_payload_max` bytes, so it can be shorter than `NodeView::latest`.
    /// An unknown id gives an empty iterator.
    pub fn history(&self, id: NodeId) -> impl DoubleEndedIterator<Item = &Msg> + '_ {
        self.live(id)
            .into_iter()
            .flat_map(|n| n.history.iter().rev().map(|h| &h.msg))
    }

    // ---------------------------------------------------------------------------------------
    // List view
    // ---------------------------------------------------------------------------------------

    /// Number of rows of the list view. The view has the filter and the pause applied.
    /// Paused: the frozen rows that the store still has, plus the removed rows, plus one marker
    /// row if the store removed any.
    pub fn list_len(&self) -> usize {
        let present = self.view_present();
        if self.paused && !self.removed.is_empty() {
            present + self.removed.len() + 1
        } else {
            present
        }
    }

    /// Row `i` of the list view. `i = 0` is the newest row. `None` if `i` is out of range.
    /// The view can change after each call that takes `&mut self`.
    pub fn list_row(&self, i: usize) -> Option<RowView<'_>> {
        let present = self.view_present();
        if i < present {
            return self.row(self.view_id(i, present)).map(|r| self.view_of(r));
        }
        if !self.paused || self.removed.is_empty() {
            return None;
        }
        let j = i - present;
        if j == 0 {
            return Some(RowView::RemovedMarker);
        }
        let k = self.removed.len().checked_sub(j)?;
        Some(RowView::Removed {
            id: self.removed[k],
        })
    }

    /// The view of a row. A node that the store removed shows as `None`.
    fn view_of<'a>(&self, row: &'a Row) -> RowView<'a> {
        let mut view = row.view();
        if let RowView::Message { node, .. } = &mut view {
            *node = node.filter(|&n| self.live(n).is_some());
        }
        view
    }

    /// True if the store removed the row (memory budget, list cap or `clear`): its id is older
    /// than the oldest row that the store keeps. False if the row is still in the list, even if
    /// a filter hides it, and for an id that the store has not issued yet. Paused: a row that is
    /// in the frozen list and that the store removed shows as `RowView::Removed`, and this is
    /// true for it.
    pub fn row_evicted(&self, id: RowId) -> bool {
        id.0 < self.front_id()
    }

    /// Index of a row in the list view (as for `list_row`), or `None` if the view does not
    /// have the row. Paused: a removed row still has its place.
    pub fn list_index_of(&self, id: RowId) -> Option<usize> {
        let present = self.view_present();
        let pos = if self.lf.active {
            self.matched.binary_search(&id).ok()
        } else {
            id.0.checked_sub(self.front_id())
                .and_then(|p| usize::try_from(p).ok())
        };
        if let Some(p) = pos
            && p < present
        {
            return Some(present - 1 - p);
        }
        if self.paused {
            let k = self.removed.binary_search(&id).ok()?;
            return Some(present + 1 + (self.removed.len() - 1 - k));
        }
        None
    }

    /// Sets the list filter. A new filter starts a rebuild (see `work`). Until the rebuild is
    /// complete, the view shows the matches among the newest rows. The filter generation changes.
    /// While paused, the pause starts again from now (see `set_paused`).
    pub fn set_list_filter(&mut self, filter: ListFilter) {
        if filter == self.lf.filter {
            return;
        }
        self.lf = ListFilterState::new(filter);
        self.filter_gen += 1;
        self.matched = VecDeque::new();
        self.scanning = self.lf.active;
        self.scan_below = self.next_row;
        self.restart_baseline();
    }

    pub fn list_filter(&self) -> &ListFilter {
        &self.lf.filter
    }

    /// Sets the tree filter. A new filter starts a rebuild (see `work`). Until the rebuild is
    /// complete, the tree shows the matches that the rebuild found. The filter generation
    /// changes. While paused, the pause starts again from now.
    pub fn set_tree_filter(&mut self, filter: TreeFilter) {
        if filter == self.tf.filter {
            return;
        }
        self.tf = TreeFilterState {
            needle: Needle::new(&filter.text),
            show: filter.show_only.as_deref().and_then(topic::Filter::new),
            active: !filter.text.is_empty() || filter.show_only.is_some(),
            filter,
        };
        self.filter_gen += 1;
        self.tree_flags = if self.tf.active {
            vec![0; self.nodes.len()]
        } else {
            Vec::new()
        };
        self.tree_cursor = 0;
        self.rows_dirty = true;
        self.restart_baseline();
    }

    pub fn tree_filter(&self) -> &TreeFilter {
        &self.tf.filter
    }

    /// Counts the filter changes (list and tree). The UI can use it to see that the view changed.
    pub fn filter_generation(&self) -> u64 {
        self.filter_gen
    }

    // ---------------------------------------------------------------------------------------
    // Work
    // ---------------------------------------------------------------------------------------

    /// True while `work` has something to do (a filter rebuild, a snapshot, or the list is above
    /// its cap).
    pub fn has_work(&self) -> bool {
        self.mem_pending()
            || self.nodes_pending()
            || self.scanning
            || self.snap_job.is_some()
            || (self.tf.active && self.tree_cursor < self.nodes.len())
            || self.list.len() > self.list_cap()
    }

    /// Does background work until `deadline`: the tree filter rebuild, the snapshot of the tree
    /// (paused), the list filter rebuild and the removal of rows above the cap (after a pause).
    /// Returns true while work remains.
    ///
    /// One round is one step of at most 256 items of the first task that has work. A step takes
    /// some microseconds and never depends on the size of the store. The call does rounds until
    /// `deadline`. If `deadline` has passed already, it does exactly one round, so that repeated
    /// calls always make progress. The UI should not call it after its frame budget is used.
    pub fn work(&mut self, deadline: Instant) -> bool {
        loop {
            if self.mem_pending() {
                self.evict_step(WORK_CHUNK);
            } else if self.nodes_pending() {
                self.evict_nodes_step(NODE_CHUNK);
            } else if self.tf.active && self.tree_cursor < self.nodes.len() {
                self.tree_step(WORK_CHUNK);
            } else if self.snap_job.is_some() {
                self.snap_step(WORK_CHUNK);
            } else if self.scanning {
                self.list_step(WORK_CHUNK);
            } else {
                self.trim_rows(WORK_CHUNK);
            }
            if !self.has_work() {
                return false;
            }
            if Instant::now() >= deadline {
                return true;
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Pause
    // ---------------------------------------------------------------------------------------

    /// Freezes (true) or unfreezes (false) the list view and the tree view.
    ///
    /// - The list view stays at the rows that were in it at the moment of the call. The store
    ///   removes a row only because of the memory budget (or after 10 times `list_len`). Such a
    ///   row shows as `RowView::Removed`, and one `RowView::RemovedMarker` shows too.
    /// - The tree view uses a snapshot of its rows with the counts and a short preview. The
    ///   snapshot is charged to the budget. This call only starts the snapshot: `work` builds it
    ///   in small steps (`has_work` is true until it is complete). Until then the tree shows the
    ///   live rows. The snapshot has the nodes that existed at the call. Its counts are the
    ///   values when `work` visits the node, so they can be newer than the call.
    /// - `paused_new` counts the new messages.
    /// - Ingest goes on inside the budget.
    pub fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        if paused {
            self.restart_baseline();
        } else {
            self.removed = Vec::new();
            self.snap_rows = Vec::new();
            self.snap = HashMap::new();
            self.snap_bytes = 0;
            self.snap_job = None;
            self.rows_dirty = true;
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Paused: the number of messages (received and sent) that the store admitted since the
    /// pause, or since the last filter change, and that match the list filter. It also counts
    /// the messages that the store removed later. It does not count drops and markers.
    pub fn paused_new(&self) -> u64 {
        self.paused_new
    }

    // ---------------------------------------------------------------------------------------
    // Subscription colours
    // ---------------------------------------------------------------------------------------

    /// Sets the subscription filters, in the order of the subscription list. This clears the
    /// colours (not when the list is the same). Invalid filters match nothing. The filters are
    /// checked and indexed once here, so a lookup does not depend on their number.
    pub fn set_subscriptions(&mut self, filters: Vec<String>) {
        if filters == self.subs.raw {
            return;
        }
        self.subs = SubIndex::new(filters);
        self.sub_rev = self.sub_rev.wrapping_add(1).max(1);
    }

    /// Index of the first subscription that matches the topic of the node. The result is cached
    /// for each node until the subscriptions change. Call it for visible rows only.
    /// An unknown node gives `None`.
    pub fn colour_of(&mut self, node: NodeId) -> Option<usize> {
        let rev = self.sub_rev;
        let subs = &self.subs;
        let n = self
            .nodes
            .get_mut(node.ix())
            .filter(|n| n.live && n.generation == node.generation)?;
        if n.colour_rev != rev {
            n.colour_rev = rev;
            n.colour_idx = subs
                .first_match(&n.topic)
                .map_or(NO_COLOUR, |i| u32::try_from(i).unwrap_or(NO_COLOUR));
        }
        (n.colour_idx != NO_COLOUR).then_some(n.colour_idx as usize)
    }

    /// Like `colour_of`, for a topic without a node (a sent message). It is not cached.
    pub fn colour_of_topic(&self, topic: &str) -> Option<usize> {
        self.subs.first_match(topic)
    }

    // ---------------------------------------------------------------------------------------

    pub fn stats(&self) -> StoreStats {
        StoreStats {
            topics: self.live_nodes,
            messages: self.messages,
            used_bytes: self.used(),
            topic_drops: self.topic_drops,
            drops: self.drops,
            history_bytes: self.hist_bytes,
            list_rows: self.list.len(),
            list_bytes: self.list_bytes,
            list_drops: self.list_drops,
        }
    }

    /// Puts a node into a free slot, or into a new one. The generation of a reused slot was
    /// raised when it was freed. While paused, free slots stay unused: the frozen tree knows the
    /// nodes by their slot.
    fn alloc(&mut self, mut node: Node) -> NodeId {
        self.live_nodes += 1;
        if !self.paused
            && let Some(idx) = self.free.pop()
        {
            node.generation = self.nodes[idx as usize].generation;
            let id = NodeId {
                idx,
                generation: node.generation,
            };
            self.nodes[idx as usize] = node;
            return id;
        }
        // `topic_cap` is at most `u32::MAX`, and a slot is never freed without a node before it.
        let idx = self.nodes.len() as u32;
        self.nodes.push(node);
        NodeId { idx, generation: 0 }
    }

    /// The node with this id, if it is still in the store.
    fn live(&self, id: NodeId) -> Option<&Node> {
        self.nodes
            .get(id.ix())
            .filter(|n| n.live && n.generation == id.generation)
    }

    fn live_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes
            .get_mut(id.ix())
            .filter(|n| n.live && n.generation == id.generation)
    }

    /// The id of the node in slot `i`, if the slot is in use.
    fn id_at(&self, i: usize) -> Option<NodeId> {
        let n = self.nodes.get(i)?;
        n.live.then_some(NodeId {
            idx: i as u32,
            generation: n.generation,
        })
    }

    /// `None` is the virtual root.
    fn children_of(&self, id: Option<NodeId>) -> &BTreeMap<Box<str>, NodeId> {
        match id {
            None => &self.roots,
            Some(id) => &self.nodes[id.ix()].children,
        }
    }

    /// All charged bytes.
    fn used(&self) -> usize {
        self.fixed() + self.latest_bytes + self.hist_bytes + self.list_bytes
    }

    /// Charged bytes that the store cannot evict.
    fn fixed(&self) -> usize {
        self.node_bytes + self.snap_bytes + self.removed.len() * size_of::<RowId>()
    }

    /// Sets a new memory budget at once (clamped to [`BUDGET_MIN`]..=[`BUDGET_MAX`]) and returns
    /// the value that is in use. The node budget becomes a quarter of it.
    ///
    /// A lower budget evicts nothing here. It changes the targets: `has_work` becomes true, and
    /// [`TopicStore::work`] brings the memory down in time slices. Until then the memory can be
    /// above the budget (the insert drops a message only above the hard ceiling, which also
    /// moves). A higher budget needs no work.
    pub fn set_budget(&mut self, bytes: usize) -> usize {
        let budget = bytes.clamp(BUDGET_MIN, BUDGET_MAX);
        self.limits.budget = budget;
        self.limits.node_budget = budget / 4;
        self.stuck_mem = false;
        self.stuck_nodes = false;
        budget
    }

    /// The memory budget in use.
    pub fn budget(&self) -> usize {
        self.limits.budget
    }

    /// The hard ceiling of the memory: the budget plus a quarter. An insert above it is dropped.
    /// The budget itself is the target of the batch eviction in [`TopicStore::work`].
    pub fn hard_limit(&self) -> usize {
        self.limits.budget + self.limits.budget / 4
    }

    /// Low watermark of the batch eviction: it stops here, so the next evictions come later.
    fn low_mark(&self) -> usize {
        self.limits.budget - self.limits.budget / 10
    }

    /// Data eviction is needed: used bytes are above the budget, or a batch is running.
    fn mem_pending(&self) -> bool {
        !self.stuck_mem && (self.evicting || self.used() > self.limits.budget)
    }

    /// One step of the data eviction: at most `n` items, oldest first, down to the low
    /// watermark. It goes on in the next call while `mem_pending`.
    fn evict_step(&mut self, n: usize) {
        self.evicting = true;
        let low = self.low_mark();
        for _ in 0..n {
            if self.used() <= low {
                self.evicting = false;
                return;
            }
            if !self.evict_one() {
                // Nothing left that can go (the rest is fixed).
                self.evicting = false;
                self.stuck_mem = self.used() > self.limits.budget;
                return;
            }
        }
    }

    /// Node eviction is needed: nodes or node bytes reached the high mark (95 % of their cap),
    /// or a batch is running. Not while paused: the frozen views hold node ids.
    fn nodes_pending(&self) -> bool {
        if self.paused || self.stuck_nodes {
            return false;
        }
        let (count_high, _) = marks(self.limits.topic_cap);
        let (bytes_high, _) = marks(self.limits.node_budget);
        self.evicting_nodes || self.live_nodes >= count_high || self.node_bytes >= bytes_high
    }

    /// One step of the node eviction: removes at most `n` idle leaves (and the empty parents
    /// above them), least recently updated first, down to 90 % of the caps.
    fn evict_nodes_step(&mut self, n: usize) {
        self.evicting_nodes = true;
        let (_, count_low) = marks(self.limits.topic_cap);
        let (_, bytes_low) = marks(self.limits.node_budget);
        for _ in 0..n {
            if self.live_nodes <= count_low && self.node_bytes <= bytes_low {
                self.evicting_nodes = false;
                return;
            }
            let victim = self.touch.values().copied().find(|&id| {
                let n = &self.nodes[id.ix()];
                // A node that is open, or that waits for the end of our own retained publish.
                !n.expanded && !self.pending_retained.iter().any(|p| p.topic == n.topic)
            });
            let Some(id) = victim else {
                self.evicting_nodes = false;
                self.stuck_nodes = true;
                return;
            };
            self.remove_leaf(id);
        }
    }

    // ponytail: list rows keep the topic `Arc` of a removed node, and nothing charges for it
    // (at most one topic of up to 64 KiB for each row). The budget is an estimate, not a strict
    // bound. Charge the row for its topic if this ever matters.
    /// Removes a leaf node, and then each parent that has no child and no message. The list rows
    /// keep their topic text but lose the node (`RowView::Message::node` is `None`). Counts of
    /// the ancestors drop by the counts of the node. `messages` (the total) does not change.
    fn remove_leaf(&mut self, mut id: NodeId) {
        loop {
            let parent = self.nodes[id.ix()].parent;
            // The rows first: the place of the node is found by its path.
            let mut was_visible = false;
            if self.tf.active {
                was_visible = self.tree_flags[id.ix()] & T_VISIBLE != 0;
                self.rows_dirty |= was_visible;
                self.tree_flags[id.ix()] = 0;
            } else if !self.rows_dirty && self.children_shown(parent) {
                match self.row_place(id) {
                    Ok(at) => {
                        self.rows.remove(at);
                    }
                    Err(_) => self.rows_dirty = true,
                }
            }
            let n = &mut self.nodes[id.ix()];
            let name = std::mem::take(&mut n.name);
            let topic = std::mem::replace(&mut n.topic, Arc::clone(&self.empty));
            let (msgs, had_msg) = (n.subtree_msgs, n.msg_count > 0);
            let cost = n.latest.take().as_ref().map_or(0, msg_cost);
            if cost > 0 {
                self.order.remove(&n.seq);
                self.latest_bytes -= cost;
            }
            for e in n.history.drain(..) {
                self.hist_order.remove(&e.seq);
                self.hist_bytes -= e.charge;
            }
            self.touch.remove(&n.touch);
            *n = Node {
                name: Box::default(),
                topic: Arc::clone(&self.empty),
                parent: None,
                depth: 0,
                children: BTreeMap::new(),
                expanded: false,
                msg_count: 0,
                subtree_msgs: 0,
                subtree_topics: 0,
                latest: None,
                seq: 0,
                history: VecDeque::new(),
                colour_rev: 0,
                colour_idx: NO_COLOUR,
                retained: None,
                generation: n.generation.wrapping_add(1),
                live: false,
                touch: 0,
            };
            self.node_bytes -= node_cost(name.len(), topic.len());
            self.free.push(id.idx);
            self.live_nodes -= 1;
            let mut up = parent;
            match parent {
                None => self.roots.remove(&*name),
                Some(p) => self.nodes[p.ix()].children.remove(&*name),
            };
            while let Some(a) = up {
                let n = &mut self.nodes[a.ix()];
                n.subtree_msgs -= msgs;
                if had_msg {
                    n.subtree_topics -= 1;
                }
                up = n.parent;
            }
            // With a tree filter, an ancestor that was open only for this node closes again. The
            // walk stops at the first ancestor that stays open.
            // ponytail: the check scans the children of such an ancestor (it stops at the first
            // visible one). A very wide parent with no visible child is one long scan.
            let mut up = parent;
            while was_visible && let Some(a) = up {
                let flags = self.tree_flags[a.ix()];
                let n = &self.nodes[a.ix()];
                let open = flags & T_MATCH != 0
                    || n.children
                        .values()
                        .any(|c| self.tree_flags[c.ix()] & T_VISIBLE != 0);
                if open {
                    break;
                }
                self.tree_flags[a.ix()] = 0;
                up = n.parent;
            }
            let Some(p) = parent else { return };
            let pn = &self.nodes[p.ix()];
            if !pn.children.is_empty() {
                return;
            }
            if pn.msg_count > 0 {
                // The parent has a message of its own and is a leaf now.
                self.touch.insert(pn.touch, p);
                return;
            }
            id = p; // an empty structure node: it goes too
        }
    }

    /// Evicts one item. The order: oldest history entry, oldest list row, oldest latest payload.
    fn evict_one(&mut self) -> bool {
        self.evict_history() || self.pop_row(true) || self.evict_latest()
    }

    /// Removes the oldest history entry of all topics. It is the front of one ring.
    fn evict_history(&mut self) -> bool {
        let Some((seq, id)) = self.hist_order.pop_first() else {
            return false;
        };
        if let Some(e) = self.nodes[id.ix()].history.pop_front() {
            debug_assert_eq!(e.seq, seq);
            self.hist_bytes -= e.charge;
        }
        true
    }

    /// Takes the bytes of the latest value of a node out of the accounting.
    /// The caller replaces the value right after.
    fn release(&mut self, id: NodeId) {
        let n = &self.nodes[id.ix()];
        let cost = n.latest.as_ref().map_or(0, msg_cost);
        if cost > 0 {
            self.order.remove(&n.seq);
            self.latest_bytes -= cost;
        }
    }

    /// Empties the oldest non-empty latest value. Returns false if there is nothing to evict.
    fn evict_latest(&mut self) -> bool {
        let Some((_, id)) = self.order.pop_first() else {
            return false;
        };
        if let Some(m) = self.nodes[id.ix()].latest.as_mut() {
            self.latest_bytes -= msg_cost(m);
            m.payload = Bytes::new();
            m.props = MsgProps::default(); // ponytail: props go too, so the cost is exactly 0
            m.truncated = true;
        }
        true
    }

    /// Adds a row to the list. The charge is already admitted. Updates the filter
    /// index and `paused_new`. Removes the rows above the cap (a few at a time).
    fn admit_row(&mut self, kind: RowKind, charge: usize, counts_new: bool) -> RowId {
        let id = RowId(self.next_row);
        self.next_row += 1;
        let row = Row { id, charge, kind };
        let m = !self.lf.active || row_matches(&row, &self.lf);
        if self.lf.active && m {
            // A rebuild scans only older rows, so this id is above all ids in the index.
            self.matched.push_back(id);
        }
        if self.paused && counts_new && m {
            self.paused_new += 1;
        }
        self.list_bytes += charge;
        self.list.push_back(row);
        self.trim_rows(MAX_TRIM_PER_INSERT);
        id
    }

    /// Removes the oldest row. `memory` is true when the budget needs the room.
    fn pop_row(&mut self, memory: bool) -> bool {
        let Some(row) = self.list.pop_front() else {
            return false;
        };
        self.list_bytes -= row.charge;
        if memory {
            self.list_drops += 1;
        }
        let in_view = !self.lf.active
            || (self.matched.front() == Some(&row.id) && self.matched.pop_front().is_some());
        if self.paused && in_view && row.id.0 < self.cutoff {
            self.removed.push(row.id);
        }
        if let RowKind::Message(MsgRow { op: Some(op), .. }) = &row.kind
            && self.ops.get(op) == Some(&row.id)
        {
            self.ops.remove(op);
        }
        true
    }

    fn list_cap(&self) -> usize {
        if self.paused {
            self.limits.list_len.saturating_mul(PAUSED_LIST_FACTOR)
        } else {
            self.limits.list_len
        }
    }

    /// Removes up to `n` rows above the cap. Returns true if rows above the cap remain.
    fn trim_rows(&mut self, n: usize) -> bool {
        let cap = self.list_cap();
        let mut k = 0;
        while self.list.len() > cap && k < n {
            self.pop_row(false);
            k += 1;
        }
        self.list.len() > cap
    }

    /// Id of the oldest row, or the next id if the list is empty.
    fn front_id(&self) -> u64 {
        self.list.front().map_or(self.next_row, |r| r.id.0)
    }

    fn row(&self, id: RowId) -> Option<&Row> {
        let i = id.0.checked_sub(self.front_id())?;
        self.list.get(usize::try_from(i).ok()?)
    }

    /// Rows of the view that the store has. Paused: only the rows from before the pause.
    fn view_present(&self) -> usize {
        match (self.lf.active, self.paused) {
            (true, false) => self.matched.len(),
            (true, true) => self.matched.partition_point(|id| id.0 < self.cutoff),
            (false, false) => self.list.len(),
            (false, true) => usize::try_from(self.cutoff.saturating_sub(self.front_id()))
                .unwrap_or(usize::MAX)
                .min(self.list.len()),
        }
    }

    /// Id of the `i`-th newest of the `present` rows of the view.
    fn view_id(&self, i: usize, present: usize) -> RowId {
        let p = present - 1 - i;
        if self.lf.active {
            self.matched[p]
        } else {
            RowId(self.front_id() + p as u64)
        }
    }

    /// The pause starts again from now. The list and the tree take a new frozen state.
    fn restart_baseline(&mut self) {
        if self.paused {
            self.cutoff = self.next_row;
            self.paused_new = 0;
            self.removed = Vec::new();
            self.start_snapshot();
        }
    }

    /// One step of the list filter rebuild. The rebuild scans from new to old, so the newest
    /// matches show first. Returns true if the scan has more rows.
    fn list_step(&mut self, n: usize) -> bool {
        if !self.scanning {
            return false;
        }
        let front = self.front_id();
        for _ in 0..n {
            if self.scan_below <= front {
                break;
            }
            self.scan_below -= 1;
            let row = usize::try_from(self.scan_below - front)
                .ok()
                .and_then(|i| self.list.get(i));
            if let Some(row) = row
                && row_matches(row, &self.lf)
            {
                self.matched.push_front(row.id);
            }
        }
        self.scanning = self.scan_below > front;
        self.scanning
    }

    /// One step of the tree filter rebuild. Returns true if nodes remain.
    fn tree_step(&mut self, n: usize) -> bool {
        if !self.tf.active {
            return false;
        }
        let start = self.tree_cursor;
        let end = (start + n).min(self.nodes.len());
        for i in start..end {
            if let Some(id) = self.id_at(i) {
                self.tree_test(id);
            }
        }
        self.tree_cursor = end;
        if self.paused && end == self.nodes.len() && end > start {
            // The frozen tree takes the result of the rebuild once, when it is complete.
            self.start_snapshot();
        }
        self.tree_tail()
    }

    fn tree_tail(&self) -> bool {
        self.tree_cursor < self.nodes.len()
    }

    /// Tests one node against the tree filter. A match and its ancestors become visible.
    fn tree_test(&mut self, id: NodeId) {
        let n = &self.nodes[id.ix()];
        let hit = self.tf.needle.found_in(&n.topic)
            && (self.tf.filter.show_only.is_none()
                || self.tf.show.as_ref().is_some_and(|s| s.matches(&n.topic)));
        if !hit {
            return;
        }
        self.tree_flags[id.ix()] |= T_MATCH | T_VISIBLE;
        // An ancestor that is open has open ancestors, so the walk stops there.
        let mut up = n.parent;
        while let Some(p) = up {
            let f = &mut self.tree_flags[p.ix()];
            if *f & T_OPEN != 0 {
                break;
            }
            *f |= T_OPEN | T_VISIBLE;
            up = self.nodes[p.ix()].parent;
        }
        self.rows_dirty = true;
    }

    /// Starts a new frozen tree view. The old one is dropped. `snap_step` builds the new one.
    fn start_snapshot(&mut self) {
        self.snap_rows = Vec::new();
        self.snap = HashMap::new();
        self.snap_bytes = 0;
        self.snap_job = Some(SnapJob {
            stack: vec![SnapFrame {
                parent: None,
                last: None,
            }],
            node_limit: self.nodes.len(),
        });
    }

    /// The child of `parent` after `last` (by name), or the first child if `last` is `None`.
    fn next_child(&self, parent: Option<NodeId>, last: Option<NodeId>) -> Option<NodeId> {
        let children = self.children_of(parent);
        let id = match last {
            None => children.values().next(),
            Some(l) => children
                .range::<str, _>((Bound::Excluded(&*self.nodes[l.ix()].name), Bound::Unbounded))
                .next()
                .map(|(_, id)| id),
        };
        id.copied()
    }

    /// One step of the snapshot: at most `n` units. A unit is one visited node (visible or not),
    /// one finished level or one evicted item. The rows are the rows of the live tree (with the
    /// same filter rules). The entries are charged to the budget. If the budget is short, the
    /// store evicts (without the cap of one insert, because this is a single action of the user)
    /// and, if that is not enough, the snapshot has fewer rows. Returns true if the job goes on.
    fn snap_step(&mut self, n: usize) -> bool {
        let Some(mut job) = self.snap_job.take() else {
            return false;
        };
        let filtered = self.tf.active;
        let mut left = n;
        let mut done = false;
        'walk: while left > 0 {
            left -= 1;
            let Some(&SnapFrame { parent, last }) = job.stack.last() else {
                done = true;
                break;
            };
            let Some(id) = self.next_child(parent, last) else {
                job.stack.pop();
                continue;
            };
            let flags = self.tree_flags.get(id.ix()).copied().unwrap_or(0);
            let skip = id.ix() >= job.node_limit || (filtered && flags & T_VISIBLE == 0);
            let mut open = false;
            if !skip {
                if self.snap_rows.len() >= SNAPSHOT_MAX_ROWS {
                    done = true;
                    break;
                }
                let node = &self.nodes[id.ix()];
                open = if filtered {
                    flags & T_OPEN != 0
                } else {
                    node.expanded
                };
                let preview: Box<str> = node.latest.as_ref().map_or_else(Box::default, |m| {
                    let head = &m.payload[..m.payload.len().min(SNAPSHOT_PREVIEW_BYTES)];
                    String::from_utf8_lossy(head).into()
                });
                let entry = SnapEntry {
                    msg_count: node.msg_count,
                    subtree_msgs: node.subtree_msgs,
                    subtree_topics: node.subtree_topics,
                    preview,
                };
                let cost = SNAP_OVERHEAD + entry.preview.len();
                while self.used() + cost > self.limits.budget {
                    if left == 0 {
                        break 'walk; // the node is not handled yet: the next step starts here
                    }
                    left -= 1;
                    if !self.evict_one() {
                        done = true;
                        break 'walk;
                    }
                }
                self.snap_bytes += cost;
                self.snap_rows.push(id);
                self.snap.insert(id, entry);
            }
            if let Some(top) = job.stack.last_mut() {
                top.last = Some(id);
            }
            if open {
                job.stack.push(SnapFrame {
                    parent: Some(id),
                    last: None,
                });
            }
        }
        if done || job.stack.is_empty() {
            return false;
        }
        self.snap_job = Some(job);
        true
    }
}

/// The 95 % and the 90 % mark of a cap (see `TopicStore::nodes_pending`). The low mark is below
/// the high mark, also for a tiny cap.
fn marks(cap: usize) -> (usize, usize) {
    (cap - cap / 20, cap - (cap / 10).max(1))
}

/// Byte offset of the end of `part` in `topic`. `part` must be a slice of `topic`.
fn prefix_end(topic: &str, part: &str) -> usize {
    part.as_ptr() as usize - topic.as_ptr() as usize + part.len()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::*;

    fn incoming(topic: &str, payload: Bytes, props: MsgProps) -> Incoming {
        Incoming {
            generation: 0,
            topic: Arc::from(topic),
            payload,
            qos: Qos::AtMostOnce,
            retain: false,
            props,
            received_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn msg(topic: &str, payload: &[u8]) -> Incoming {
        incoming(topic, Bytes::copy_from_slice(payload), MsgProps::default())
    }

    fn big() -> Limits {
        Limits::default()
    }

    /// Limits for a test with 100k nodes (the default node budget is smaller).
    fn many_nodes() -> Limits {
        Limits {
            budget: 512 * 1024 * 1024,
            node_budget: 256 * 1024 * 1024,
            ..big()
        }
    }

    fn stored(r: Insert) -> NodeId {
        match r {
            Insert::Stored(id) => id,
            other => panic!("expected Stored, got {other:?}"),
        }
    }

    fn sent(s: &mut TopicStore, op: u64, topic: &str, payload: &[u8]) -> Insert {
        s.record_sent(
            op,
            topic,
            Bytes::copy_from_slice(payload),
            Qos::AtLeastOnce,
            false,
            MsgProps::default(),
            SystemTime::UNIX_EPOCH,
        )
    }

    fn row_paths(s: &mut TopicStore) -> Vec<String> {
        let rows = s.rows().to_vec();
        rows.into_iter().map(|id| s.path(id)).collect()
    }

    /// Charge of one message with the default limits and no properties: latest, history, row.
    fn per_msg(len: usize) -> usize {
        len + HIST_OVERHEAD + len.min(big().history_payload_max) + ROW_OVERHEAD + len
    }

    /// Charge of a root node.
    fn root_cost(name: &str) -> usize {
        node_cost(name.len(), name.len())
    }

    /// Runs `work` with a deadline in the past until it is done. Each call does one step.
    fn drain(s: &mut TopicStore) -> usize {
        let mut calls = 1;
        while s.work(Instant::now()) {
            calls += 1;
        }
        calls
    }

    /// Ids of the list view, newest first. A marker or a removed row is easy to see in the tests.
    fn view_ids(s: &TopicStore) -> Vec<u64> {
        (0..s.list_len())
            .map(|i| match s.list_row(i).expect("row in range") {
                RowView::Message { id, .. }
                | RowView::Reconnected { id, .. }
                | RowView::Removed { id } => id.0,
                RowView::RemovedMarker => u64::MAX,
            })
            .collect()
    }

    fn payload_of(s: &TopicStore, i: usize) -> Vec<u8> {
        match s.list_row(i).expect("row") {
            RowView::Message { msg, .. } => msg.payload.to_vec(),
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// Reference filter, written the slow way. Newest first.
    fn naive_ids(s: &TopicStore, f: &ListFilter) -> Vec<u64> {
        let text = f.text.to_lowercase();
        s.list
            .iter()
            .rev()
            .filter(|r| match &r.kind {
                RowKind::Reconnected(_) => true,
                RowKind::Message(m) => {
                    let dir = match f.direction {
                        DirectionFilter::All => true,
                        DirectionFilter::Received => m.direction == Direction::Received,
                        DirectionFilter::Sent => m.direction == Direction::Sent,
                    };
                    let p = &m.msg.payload;
                    let head = String::from_utf8_lossy(&p[..p.len().min(1024)]).to_lowercase();
                    dir && f
                        .show_only
                        .as_ref()
                        .is_none_or(|x| topic::matches(x, &m.topic))
                        && (text.is_empty()
                            || m.topic.to_lowercase().contains(&text)
                            || head.contains(&text))
                }
            })
            .map(|r| r.id.0)
            .collect()
    }

    fn naive_tree(f: &TreeFilter, topic: &str) -> bool {
        (f.text.is_empty() || topic.to_lowercase().contains(&f.text.to_lowercase()))
            && f.show_only
                .as_ref()
                .is_none_or(|x| topic::matches(x, topic))
    }

    /// Recomputes every counter from the content and compares it with the store.
    fn audit(s: &TopicStore) {
        let lim = s.limits;
        let (mut nodes, mut latest, mut hist, mut hist_n) = (0, 0, 0, 0);
        let free: HashSet<u32> = s.free.iter().copied().collect();
        assert_eq!(free.len(), s.free.len(), "no slot is free twice");
        let mut live_count = 0;
        for (i, n) in s.nodes.iter().enumerate() {
            let id = NodeId {
                idx: i as u32,
                generation: n.generation,
            };
            if !n.live {
                assert!(
                    free.contains(&(i as u32)),
                    "a dead slot is in the free list"
                );
                assert!(n.latest.is_none() && n.history.is_empty() && n.children.is_empty());
                assert!(s.live(id).is_none());
                continue;
            }
            assert!(!free.contains(&(i as u32)));
            live_count += 1;
            nodes += node_cost(n.name.len(), n.topic.len());
            // Counts of the subtree, from the children.
            let (mut msgs, mut topics) = (n.msg_count, 0u32);
            let mut stack: Vec<NodeId> = n.children.values().copied().collect();
            while let Some(c) = stack.pop() {
                let cn = s.live(c).expect("a child is live");
                msgs += cn.msg_count;
                topics += u32::from(cn.msg_count > 0);
                stack.extend(cn.children.values().copied());
            }
            assert_eq!(n.subtree_msgs, msgs, "subtree_msgs of {}", n.topic);
            assert_eq!(n.subtree_topics, topics, "subtree_topics of {}", n.topic);
            for c in n.children.values() {
                assert_eq!(s.live(*c).expect("a child is live").parent, Some(id));
            }
            // A leaf has a message and is in the touch index. Nothing else is.
            if n.children.is_empty() {
                assert!(n.msg_count > 0, "an empty leaf must not exist: {}", n.topic);
                assert_eq!(s.touch.get(&n.touch), Some(&id));
            } else {
                assert!(!s.touch.contains_key(&n.touch) || n.touch == 0);
            }
            let mut names = vec![&*n.name];
            let mut p = n.parent;
            while let Some(q) = p {
                names.push(&s.nodes[q.ix()].name);
                p = s.nodes[q.ix()].parent;
            }
            names.reverse();
            assert_eq!(names.join("/"), *n.topic, "topic is the path");
            latest += n.latest.as_ref().map_or(0, msg_cost);
            assert!(n.history.len() <= lim.history_len);
            let mut last = 0;
            for e in &n.history {
                let expect = HIST_OVERHEAD + e.msg.payload.len() + e.msg.props.heap_bytes();
                assert_eq!(e.charge, expect, "recorded history charge");
                assert!(e.msg.payload.len() <= lim.history_payload_max);
                assert!(e.seq > last);
                last = e.seq;
                assert_eq!(s.hist_order.get(&e.seq), Some(&id));
                hist += e.charge;
                hist_n += 1;
            }
        }
        if !s.rows_dirty {
            assert_eq!(
                s.rows,
                s.rebuild_rows(),
                "spliced rows equal a full rebuild"
            );
        }
        assert_eq!(s.live_nodes, live_count);
        assert_eq!(
            s.touch.len(),
            s.nodes
                .iter()
                .filter(|n| n.live && n.children.is_empty())
                .count()
        );
        assert_eq!(s.node_bytes, nodes);
        assert_eq!(s.latest_bytes, latest);
        assert_eq!(s.hist_bytes, hist);
        assert_eq!(s.hist_order.len(), hist_n);
        let live = s
            .nodes
            .iter()
            .filter(|n| n.live && n.latest.as_ref().is_some_and(|m| msg_cost(m) > 0));
        assert_eq!(s.order.len(), live.count());
        for (seq, id) in &s.order {
            assert_eq!(s.nodes[id.ix()].seq, *seq);
        }

        // List.
        let front = s.front_id();
        let mut list = 0;
        for (i, r) in s.list.iter().enumerate() {
            assert_eq!(r.id.0, front + i as u64, "ids are consecutive");
            let expect = match &r.kind {
                RowKind::Reconnected(_) => ROW_OVERHEAD,
                RowKind::Message(m) => {
                    let base = ROW_OVERHEAD + m.msg.payload.len() + m.msg.props.heap_bytes();
                    match m.direction {
                        Direction::Received => base,
                        Direction::Sent => {
                            let own_topic = m.topic.len() + ARC_OVERHEAD;
                            if m.node.is_some_and(|n| s.live(n).is_none()) {
                                // The node went away after the row was recorded: either charge.
                                assert!(
                                    r.charge == base + SENT_EXTRA
                                        || r.charge == base + SENT_EXTRA + own_topic
                                );
                                r.charge
                            } else {
                                base + SENT_EXTRA + if m.node.is_none() { own_topic } else { 0 }
                            }
                        }
                    }
                }
            };
            assert_eq!(r.charge, expect, "recorded row charge");
            list += r.charge;
        }
        assert_eq!(s.list_bytes, list);
        assert_eq!(s.next_row, front + s.list.len() as u64);
        assert!(s.list.len() <= s.list_cap() || s.has_work());
        for (op, id) in &s.ops {
            match &s.row(*id).expect("op row exists").kind {
                RowKind::Message(m) => assert_eq!(m.op, Some(*op)),
                RowKind::Reconnected(_) => panic!("op points to a marker"),
            }
        }

        // The filter index.
        if s.lf.active {
            assert!(s.matched.iter().is_sorted());
            assert!(s.matched.iter().all(|id| s.row(*id).is_some()));
            let truth = naive_ids(s, &s.lf.filter);
            let have: Vec<u64> = s.matched.iter().rev().map(|id| id.0).collect();
            if s.scanning {
                assert!(have.iter().all(|id| truth.contains(id)), "no false match");
                let tail = truth.iter().filter(|id| **id >= s.scan_below);
                assert!(
                    tail.clone().all(|id| have.contains(id)),
                    "scanned part complete"
                );
            } else {
                assert_eq!(have, truth, "index equals full recompute");
            }
        } else {
            assert!(s.matched.is_empty() && !s.scanning);
        }

        // The tree filter.
        if s.tf.active {
            assert_eq!(s.tree_flags.len(), s.nodes.len());
            let c = s.tree_cursor;
            let mut want = vec![0u8; c];
            for i in 0..c {
                let n = &s.nodes[i];
                if n.live && naive_tree(&s.tf.filter, &n.topic) {
                    want[i] |= T_MATCH | T_VISIBLE;
                    let mut up = n.parent;
                    while let Some(p) = up {
                        want[p.ix()] |= T_OPEN | T_VISIBLE;
                        up = s.nodes[p.ix()].parent;
                    }
                }
            }
            assert_eq!(&s.tree_flags[..c], &want[..]);
            assert!(s.tree_flags[c..].iter().all(|&f| f == 0));
        } else {
            assert!(s.tree_flags.is_empty());
        }

        // Pause.
        let snap: usize = s
            .snap
            .values()
            .map(|e| SNAP_OVERHEAD + e.preview.len())
            .sum();
        assert_eq!(s.snap_bytes, snap);
        assert_eq!(s.snap.len(), s.snap_rows.len());
        assert!(s.removed.is_empty() || s.paused);
        assert!(s.removed.iter().is_sorted());
        assert!(s.removed.iter().all(|id| id.0 < front && id.0 < s.cutoff));

        let total = nodes + latest + hist + list + snap + s.removed.len() * size_of::<RowId>();
        assert_eq!(s.stats().used_bytes, total);
        assert!(total <= s.hard_limit(), "over the hard ceiling");
        assert!(s.node_bytes <= lim.node_budget);
    }

    /// A tiny random number generator. No dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn creates_path_and_counts() {
        let mut s = TopicStore::new(big());
        let c = stored(s.insert(msg("a/b/c", b"1")));
        s.insert(msg("a/b/c", b"2"));
        let d = stored(s.insert(msg("a/b/d", b"3")));
        s.insert(msg("a/x", b"4"));

        assert_eq!(s.find("a/b/c"), Some(c));
        assert_eq!(s.path(d), "a/b/d");
        assert_eq!(s.find("a/b/zz"), None);
        let a = s.find("a").expect("a exists");
        let b = s.find("a/b").expect("a/b exists");
        assert_eq!(s.parent(c), Some(b));
        assert_eq!(s.parent(a), None);

        let nc = s.node(c);
        assert_eq!(
            (nc.msg_count, nc.subtree_msgs, nc.subtree_topics, nc.depth),
            (2, 2, 0, 2)
        );
        assert_eq!(nc.latest.expect("latest").payload, Bytes::from_static(b"2"));
        assert_eq!(nc.topic, "a/b/c");
        let nb = s.node(b);
        assert_eq!(
            (nb.msg_count, nb.subtree_msgs, nb.subtree_topics),
            (0, 3, 2)
        );
        assert!(nb.has_children && !nb.expanded && nb.latest.is_none());
        let na = s.node(a);
        assert_eq!((na.subtree_msgs, na.subtree_topics, na.depth), (4, 3, 0));

        // A message on an inner node counts it as a topic once.
        s.insert(msg("a/b", b"i"));
        s.insert(msg("a/b", b"i"));
        assert_eq!(s.node(a).subtree_topics, 4);
        assert_eq!(s.node(a).subtree_msgs, 6);
        let st = s.stats();
        assert_eq!((st.topics, st.messages), (5, 6));
        audit(&s);
    }

    #[test]
    fn rows_sorted_and_follow_expansion() {
        let mut s = TopicStore::new(big());
        for t in ["b/y", "a/z", "a/b", "c"] {
            s.insert(msg(t, b"x"));
        }
        assert_eq!(row_paths(&mut s), ["a", "b", "c"]);
        let a = s.find("a").expect("a");
        s.set_expanded(a, true);
        assert_eq!(row_paths(&mut s), ["a", "a/b", "a/z", "b", "c"]);
        // A new visible node shows up.
        s.insert(msg("a/aa", b"x"));
        s.insert(msg("0", b"x"));
        assert_eq!(
            row_paths(&mut s),
            ["0", "a", "a/aa", "a/b", "a/z", "b", "c"]
        );
        // A new node below a collapsed node does not change the rows.
        s.insert(msg("b/y/deep", b"x"));
        assert_eq!(row_paths(&mut s).len(), 7);
        // Nested expansion, and collapse again.
        let ab = s.find("a/b").expect("a/b");
        s.insert(msg("a/b/q", b"x"));
        s.set_expanded(ab, true);
        assert_eq!(row_paths(&mut s)[3], "a/b");
        assert_eq!(row_paths(&mut s)[4], "a/b/q");
        s.set_expanded(a, false);
        assert_eq!(row_paths(&mut s), ["0", "a", "b", "c"]);
        s.set_expanded(NodeId::raw(9999), true); // unknown id is ignored
        assert_eq!(s.path(NodeId::raw(9999)), "");
        assert_eq!(s.parent(NodeId::raw(9999)), None);
    }

    #[test]
    fn empty_levels_and_dollar_topics() {
        let mut s = TopicStore::new(big());
        let ab = stored(s.insert(msg("a//b", b"1")));
        let la = stored(s.insert(msg("/a", b"2")));
        stored(s.insert(msg("$SYS/broker/uptime", b"3")));
        stored(s.insert(msg("trailing/", b"4")));
        assert_eq!(s.path(ab), "a//b");
        assert_eq!(s.path(la), "/a");
        assert_eq!(s.node(ab).depth, 2);
        assert_eq!(s.node(la).name, "a");
        let empty = s.find("").expect("empty root level");
        assert_eq!(s.node(empty).name, "");
        assert!(s.find("a/").is_some());
        assert!(s.find("trailing/").is_some());
        assert!(s.find("$SYS/broker").is_some());
        // "" sorts first, then "$SYS", "a", "trailing".
        assert_eq!(row_paths(&mut s), ["", "$SYS", "a", "trailing"]);
        s.set_expanded(empty, true);
        assert_eq!(row_paths(&mut s)[..2], ["", "/a"]);
        audit(&s);
    }

    #[test]
    fn truncation_copies() {
        let limits = Limits {
            latest_payload_max: 4,
            ..big()
        };
        let mut s = TopicStore::new(limits);
        let long = Bytes::from_static(b"0123456789");
        let id = stored(s.insert(incoming("t", long.clone(), MsgProps::default())));
        let m = s.node(id).latest.expect("latest");
        assert_eq!(&m.payload[..], b"0123");
        assert!(m.truncated);
        assert_eq!(m.original_size, 10);
        assert_ne!(m.payload.as_ptr(), long.as_ptr(), "must be a copy");
        assert_eq!(s.stats().used_bytes, root_cost("t") + per_msg(4));

        // Exactly at the limit: kept as is, same buffer in all three places.
        let exact = Bytes::from_static(b"abcd");
        let id = stored(s.insert(incoming("t", exact.clone(), MsgProps::default())));
        let m = s.node(id).latest.expect("latest");
        assert!(!m.truncated);
        assert_eq!(m.payload.as_ptr(), exact.as_ptr());
        assert_eq!(
            s.history(id).next().expect("history").payload.as_ptr(),
            exact.as_ptr()
        );
        match s.list_row(0).expect("row") {
            RowView::Message { msg, .. } => assert_eq!(msg.payload.as_ptr(), exact.as_ptr()),
            other => panic!("{other:?}"),
        }
        audit(&s);
    }

    #[test]
    fn topic_cap_drops_new_topics_only() {
        let mut s = TopicStore::new(Limits {
            topic_cap: 2,
            ..big()
        });
        stored(s.insert(msg("a/b", b"1"))); // 2 nodes
        assert_eq!(s.insert(msg("c", b"1")), Insert::TopicDropped);
        assert_eq!(s.insert(msg("a/b/c", b"1")), Insert::TopicDropped);
        assert_eq!(s.find("a/b/c"), None);
        assert_eq!(s.find("c"), None);
        stored(s.insert(msg("a/b", b"2")));
        stored(s.insert(msg("a", b"2"))); // existing inner node
        let st = s.stats();
        assert_eq!(
            (st.topics, st.topic_drops, st.drops, st.messages),
            (2, 2, 0, 3)
        );
        assert_eq!(st.list_rows, 3, "a dropped message adds no row");
        audit(&s);
    }

    #[test]
    fn node_budget_drops_new_topics() {
        let limits = Limits {
            node_budget: 2 * root_cost("x"),
            ..big()
        };
        let mut s = TopicStore::new(limits);
        stored(s.insert(msg("x", b"1")));
        stored(s.insert(msg("y", b"1")));
        assert_eq!(s.insert(msg("z", b"1")), Insert::TopicDropped);
        assert_eq!(s.insert(msg("x/y", b"1")), Insert::TopicDropped);
        assert_eq!(s.stats().topics, 2);
        assert_eq!(s.stats().topic_drops, 2);
        stored(s.insert(msg("y", b"2")));
        audit(&s);
    }

    #[test]
    fn budget_is_never_exceeded() {
        let limits = Limits {
            latest_payload_max: 8 * 1024,
            topic_cap: 400,
            budget: 96 * 1024,
            node_budget: 32 * 1024,
            history_len: 5,
            history_payload_max: 1024,
            list_len: 50,
        };
        let mut s = TopicStore::new(limits);
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let (mut stored_n, mut topic_dropped, mut dropped) = (0, 0, 0);
        for i in 0..4000 {
            let t = rng.below(500);
            let topic = format!(
                "{}/{}/{}",
                "site-with-a-rather-long-name".repeat(1 + t % 3),
                t % 7,
                format!("device-{t}-").repeat(1 + t % 5)
            );
            let len = match rng.below(4) {
                0 => 0,
                1 => 1,
                2 => rng.below(500),
                _ => rng.below(40_000),
            };
            let props = MsgProps {
                content_type: (i % 5 == 0).then(|| {
                    // Now and then a huge property, so that some messages never fit.
                    let max = if i % 50 == 0 { 120_000 } else { 2000 };
                    "x".repeat(rng.below(max))
                }),
                ..MsgProps::default()
            };
            match s.insert(incoming(&topic, Bytes::from(vec![7u8; len]), props)) {
                Insert::Stored(_) => stored_n += 1,
                Insert::TopicDropped => topic_dropped += 1,
                Insert::Dropped => dropped += 1,
                Insert::Recorded(_) => panic!("insert never records"),
            }
            assert!(
                s.stats().used_bytes <= s.hard_limit(),
                "over the hard ceiling at insert {i}"
            );
            if i % 50 == 0 {
                settle(&mut s);
                assert!(s.stats().used_bytes <= limits.budget, "over budget at {i}");
            }
            if i % 97 == 0 {
                audit(&s);
            }
        }
        audit(&s);
        assert!(stored_n > 0 && topic_dropped > 0 && dropped > 0);
        let st = s.stats();
        assert_eq!((st.topic_drops, st.drops), (topic_dropped, dropped));
        assert!(st.list_drops > 0);
    }

    #[test]
    fn eviction_keeps_metadata() {
        let n = root_cost("a"); // "b" costs the same
        let limits = Limits {
            budget: 2 * n + 2 * per_msg(600) - 50,
            node_budget: 6 * n,
            ..big()
        };
        let mut s = TopicStore::new(limits);
        let a = stored(s.insert(msg("a", &[1; 600])));
        let b = stored(s.insert(msg("b", &[2; 600])));
        // Both fit below the hard ceiling. The batch then removes the oldest history entry.
        assert!(s.has_work());
        settle(&mut s);
        assert!(s.stats().used_bytes <= limits.budget);
        assert_eq!(s.find("a"), Some(a));
        assert_eq!(s.node(a).msg_count, 1);
        assert_eq!(s.history(a).count(), 0);
        assert_eq!(s.history(b).count(), 1);
        assert_eq!(s.node(a).latest.expect("a").payload.len(), 600);
        assert_eq!(s.stats().list_drops, 0);
        audit(&s);

        // Past the hard ceiling the message is dropped. The old value stays.
        assert_eq!(s.insert(msg("b", &[3; 5000])), Insert::Dropped);
        assert_eq!(s.node(b).latest.expect("b").payload.len(), 600);
        assert_eq!(s.stats().drops, 1);
        audit(&s);
        // A smaller value fits: the old cost counts as free space.
        stored(s.insert(msg("b", &[3; 500])));
        assert_eq!(s.node(b).latest.expect("b").payload.len(), 500);
        audit(&s);

        // Eviction of the payloads keeps the message record (and the node).
        let mut t = TopicStore::new(limits);
        let a = stored(t.insert(msg("a", &[1; 600])));
        stored(t.insert(msg("b", &[2; 600])));
        t.limits.budget = 2 * n + 10; // memory pressure: everything must go
        settle(&mut t);
        let ma = t.node(a).latest.expect("a keeps its message record");
        assert!(ma.payload.is_empty() && ma.truncated);
        assert_eq!(ma.original_size, 600);
        assert_eq!(t.node(a).msg_count, 1);
        assert_eq!(t.history(a).count(), 0);
        audit(&t);
    }

    #[test]
    fn eviction_order_is_history_then_list_then_latest() {
        let limits = Limits {
            budget: 300 * 1024,
            node_budget: 100 * 1024,
            ..big()
        };
        let mut s = TopicStore::new(limits);
        let emptied = |s: &TopicStore| {
            s.nodes
                .iter()
                .filter(|n| {
                    n.latest
                        .as_ref()
                        .is_some_and(|m| m.truncated && m.payload.is_empty())
                })
                .count()
        };
        let (mut hist_gone, mut list_gone, mut latest_gone) = (false, false, false);
        for i in 0..400 {
            let before = emptied(&s);
            let had = (s.hist_order.len(), s.list.len());
            s.insert(msg(&format!("t/{}", i % 150), &[1u8; 3000]));
            settle(&mut s);
            assert!(s.stats().used_bytes <= limits.budget);
            if emptied(&s) > before {
                // A latest value was evicted: the batch had removed all history and rows first.
                assert_eq!((s.hist_order.len(), s.list.len()), (0, 0), "at {i}");
                latest_gone = true;
            } else if s.list.len() < had.1 + 1 {
                // A list row was evicted: the batch had removed all history first.
                assert_eq!(s.hist_order.len(), 0, "at {i}");
                list_gone = true;
            } else if s.hist_order.len() < had.0 + 1 {
                // Only history was evicted: no row left the list.
                assert_eq!(s.list.len(), had.1 + 1, "at {i}");
                hist_gone = true;
            }
            audit(&s);
        }
        assert!(hist_gone && list_gone && latest_gone);
    }

    /// Many topics with repeated messages. Node metadata, the latest values and the full list fit
    /// in the budget, but the history does not. The history goes. The list and the latest values
    /// stay.
    #[test]
    fn history_is_evicted_before_the_list() {
        const TOPICS: usize = 200;
        let topic = |i: usize| format!("t/{}", i % TOPICS);
        let payload = |n: u64| {
            let mut p = n.to_le_bytes().to_vec();
            p.resize(100, b'x');
            p
        };
        let mut reference = TopicStore::new(big());
        for i in 0..TOPICS {
            stored(reference.insert(msg(&topic(i), &payload(0))));
        }
        let list_len = 100;
        let budget = reference.node_bytes
            + TOPICS * 100
            + list_len * (ROW_OVERHEAD + 100)
            + 20 * (HIST_OVERHEAD + 100);
        // The batch frees a tenth of the budget, so the budget has room for that, too.
        let budget = budget + budget / 8;
        let mut s = TopicStore::new(Limits {
            budget,
            list_len,
            ..big()
        });
        let total = TOPICS * 20;
        for i in 0..total {
            stored(s.insert(msg(&topic(i), &payload(i as u64))));
            settle(&mut s);
            assert!(s.stats().used_bytes <= budget, "over budget at {i}");
            if i % 97 == 0 {
                audit(&s);
            }
        }
        audit(&s);
        let st = s.stats();
        assert_eq!(
            (st.drops, st.list_drops),
            (0, 0),
            "no row left because of memory"
        );
        assert!(
            st.history_bytes <= 40 * (HIST_OVERHEAD + 100),
            "history was evicted"
        );
        assert!(s.hist_order.len() <= 40 && s.hist_order.len() < total / 10);
        // The list is full, newest first.
        assert_eq!(s.list_len(), list_len);
        for i in 0..list_len {
            let want = (total - 1 - i) as u64;
            assert_eq!(payload_of(&s, i)[..8], want.to_le_bytes());
        }
        // Every latest value is intact.
        for n in &s.nodes {
            if let Some(m) = &n.latest {
                assert!(!m.truncated && m.payload.len() == 100);
            }
        }
    }

    #[test]
    fn empty_and_one_byte_payloads() {
        let n = root_cost("a");
        let mut s = TopicStore::new(Limits {
            budget: 2 * n + 4 * per_msg(1),
            node_budget: 4 * n,
            ..big()
        });
        for i in 0..300 {
            let p: &[u8] = if i % 3 == 0 { b"" } else { b"x" };
            stored(s.insert(msg("a", p)));
            settle(&mut s);
            stored(s.insert(msg("b", p)));
            settle(&mut s);
            audit(&s);
        }
        assert_eq!(s.stats().drops, 0);

        // Exactly one empty message fits under the hard ceiling. One byte less does not.
        let hard = n + per_msg(0);
        let budget = (0..).find(|&b: &usize| b + b / 4 >= hard).expect("budget");
        let mut t = TopicStore::new(Limits {
            budget: budget - 1,
            node_budget: n,
            ..big()
        });
        assert_eq!(t.insert(msg("a", b"")), Insert::Dropped);
        assert_eq!(t.stats().topics, 0, "a dropped message creates no node");
        let mut t = TopicStore::new(Limits {
            budget,
            node_budget: n,
            ..big()
        });
        stored(t.insert(msg("a", b"")));
        audit(&t);
        assert_eq!(t.stats().used_bytes, n + per_msg(0));
    }

    #[test]
    fn zero_byte_payloads_in_all_views() {
        let mut s = TopicStore::new(big());
        let a = stored(s.insert(msg("a", b"")));
        sent(&mut s, 1, "a", b"");
        assert_eq!(s.history(a).next().expect("history").payload.len(), 0);
        assert_eq!(payload_of(&s, 1), b"");
        s.set_list_filter(ListFilter {
            text: "a".into(),
            ..ListFilter::default()
        });
        drain(&mut s);
        assert_eq!(s.list_len(), 2, "the topic matches");
        s.set_list_filter(ListFilter {
            text: "zz".into(),
            ..ListFilter::default()
        });
        drain(&mut s);
        assert_eq!(s.list_len(), 0);
        audit(&s);
    }

    #[test]
    fn clear_resets_everything() {
        let mut s = TopicStore::new(Limits {
            topic_cap: 1,
            ..big()
        });
        stored(s.insert(msg("a", b"1")));
        assert_eq!(s.insert(msg("b", b"1")), Insert::TopicDropped);
        let a = s.find("a").expect("a");
        s.set_expanded(a, true);
        assert_eq!(s.rows().len(), 1);
        sent(&mut s, 7, "a", b"x");
        s.set_list_filter(ListFilter {
            text: "a".into(),
            ..ListFilter::default()
        });
        s.set_paused(true);
        let old = RowId(s.next_row - 1);
        s.clear();
        assert_eq!(s.stats(), StoreStats::default());
        assert!(s.rows().is_empty());
        assert_eq!(s.find("a"), None);
        assert!(!s.is_paused());
        assert_eq!(s.list_len(), 0);
        assert!(!s.set_send_status(7, SendStatus::Sent));
        assert_eq!(stored(s.insert(msg("z", b"1"))), NodeId::raw(0));
        assert_eq!(
            s.insert(msg("q", b"1")),
            Insert::TopicDropped,
            "limits survive clear"
        );
        // The filter survives. A row id is never reused.
        assert_eq!(s.list_filter().text, "a");
        assert!(s.next_row > old.0 + 1);
        assert_eq!(s.list_index_of(old), None);
        audit(&s);
    }

    #[test]
    fn bad_limits_are_fixed() {
        let mut s = TopicStore::new(Limits {
            latest_payload_max: 0,
            topic_cap: 0,
            budget: 0,
            node_budget: 0,
            history_len: 0,
            history_payload_max: 0,
            list_len: 0,
        });
        assert_eq!(s.limits, big());
        stored(s.insert(msg("a", b"1")));
        let small = root_cost("a") - 1;
        let mut s = TopicStore::new(Limits {
            budget: small,
            node_budget: small + 5000,
            ..big()
        });
        assert_eq!(s.limits.node_budget, small);
        assert_eq!(s.insert(msg("a", b"1")), Insert::TopicDropped);
        let s = TopicStore::new(Limits {
            topic_cap: usize::MAX,
            ..big()
        });
        assert_eq!(s.limits.topic_cap, u32::MAX as usize);
    }

    #[test]
    fn many_messages_many_topics() {
        let mut s = TopicStore::new(big());
        for i in 0..100_000u32 {
            let t = i % 10_000;
            let topic = format!("site{}/line{}/dev{}", t % 10, (t / 10) % 10, t / 100);
            stored(s.insert(msg(&topic, &i.to_le_bytes())));
        }
        let st = s.stats();
        assert_eq!(st.messages, 100_000);
        assert_eq!(st.topics, 10 + 100 + 10_000);
        assert_eq!(st.list_rows, 10_000);
        assert_eq!(st.list_drops, 0, "the cap is not a memory drop");
        let site = s.find("site0").expect("site0");
        assert_eq!(s.node(site).subtree_msgs, 10_000);
        assert_eq!(s.node(site).subtree_topics, 1000);
        s.set_expanded(site, true);
        assert_eq!(s.rows().len(), 10 + 10);
        audit(&s);
    }

    #[test]
    fn too_deep_topic_is_dropped() {
        let mut st = TopicStore::new(Limits::default());
        let deep = vec!["a"; MAX_TOPIC_LEVELS + 1].join("/");
        assert_eq!(st.insert(msg(&deep, b"x")), Insert::TopicDropped);
        assert_eq!(st.stats().topic_drops, 1);
        let ok = vec!["a"; MAX_TOPIC_LEVELS].join("/");
        assert!(matches!(st.insert(msg(&ok, b"x")), Insert::Stored(_)));
        assert!(st.get(NodeId::raw(u32::MAX)).is_none());
        audit(&st);
    }

    fn mid_limits() -> Limits {
        Limits {
            topic_cap: 10_000,
            budget: 4 * 1024 * 1024,
            node_budget: 2 * 1024 * 1024,
            ..big()
        }
    }

    /// Adds small values (no eviction runs) until the used bytes reach `target`.
    fn fill_tiny(st: &mut TopicStore, target: usize) {
        let mut i = 0;
        while st.stats().used_bytes < target && i < 20_000 {
            st.insert(msg(&format!("t/{i}"), &[0u8; 600]));
            i += 1;
        }
    }

    /// Runs `work` until it has nothing to do.
    fn settle(s: &mut TopicStore) {
        while s.work(Instant::now() + Duration::from_secs(5)) {}
    }

    #[test]
    fn a_big_message_is_stored_at_once_and_work_evicts_in_a_batch() {
        let limits = mid_limits();
        let mut st = TopicStore::new(limits);
        fill_tiny(&mut st, limits.budget - 64 * 1024);
        assert!(!st.has_work());
        let big = vec![1u8; 400 * 1024];
        let drops = st.stats().drops;
        // The insert does no eviction. The message fits below the hard ceiling.
        assert!(matches!(st.insert(msg("big", &big)), Insert::Stored(_)));
        assert_eq!(st.stats().drops, drops);
        let over = st.stats().used_bytes;
        assert!(over > limits.budget && over <= st.hard_limit(), "{over}");
        assert!(st.has_work());
        settle(&mut st);
        let used = st.stats().used_bytes;
        assert!(used <= limits.budget - limits.budget / 10, "{used}");
        assert!(!st.has_work());
        let id = st.find("big").expect("node");
        assert_eq!(st.node(id).latest.expect("value").payload.len(), big.len());
        audit(&st);
    }

    #[test]
    fn an_insert_above_the_hard_ceiling_is_dropped() {
        let limits = mid_limits();
        let mut st = TopicStore::new(limits);
        let big = vec![1u8; 256 * 1024];
        let mut stored = 0;
        let mut first_drop = None;
        for i in 0..12 {
            match st.insert(msg(&format!("big/{i}"), &big)) {
                Insert::Stored(_) => stored += 1,
                Insert::Dropped => {
                    first_drop.get_or_insert(i);
                }
                other => panic!("{other:?}"),
            }
            assert!(st.stats().used_bytes <= st.hard_limit());
        }
        assert!(
            stored >= 5 && first_drop.is_some(),
            "{stored} {first_drop:?}"
        );
        assert!(st.stats().drops > 0);
        settle(&mut st);
        assert!(st.stats().used_bytes <= limits.budget);
        // After the batch there is room again.
        assert!(matches!(st.insert(msg("big/x", &big)), Insert::Stored(_)));
        audit(&st);
    }

    #[test]
    fn failed_replacement_keeps_the_old_value() {
        let limits = mid_limits();
        let mut st = TopicStore::new(limits);
        st.insert(msg("first", b"old value"));
        let target = st.hard_limit() - 64 * 1024;
        fill_tiny(&mut st, target);
        // Above the hard ceiling: the replacement is dropped and the old value stays.
        let big = vec![1u8; 1024 * 1024];
        assert_eq!(st.insert(msg("first", &big)), Insert::Dropped);
        let id = st.find("first").expect("node");
        let old = st.node(id).latest.expect("value");
        assert_eq!(&old.payload[..], b"old value");
        assert!(!old.truncated);
        assert!(st.stats().used_bytes <= st.hard_limit());
        audit(&st);
    }

    #[test]
    fn history_ring_and_truncation() {
        let mut s = TopicStore::new(Limits {
            history_len: 3,
            history_payload_max: 4,
            ..big()
        });
        let id = stored(s.insert(msg("t", b"0")));
        for i in 1..5u8 {
            s.insert(msg("t", &[b'0' + i]));
        }
        let h: Vec<_> = s.history(id).map(|m| m.payload[0]).collect();
        assert_eq!(h, [b'4', b'3', b'2'], "newest first, three entries");
        let rev: Vec<_> = s.history(id).rev().map(|m| m.payload[0]).collect();
        assert_eq!(rev, [b'2', b'3', b'4']);
        assert_eq!(s.stats().history_bytes, 3 * (HIST_OVERHEAD + 1));

        // A long payload is truncated in the history (a copy), not in the latest value.
        let long = Bytes::from_static(b"0123456789");
        s.insert(incoming("t", long.clone(), MsgProps::default()));
        let first = s.history(id).next().expect("newest");
        assert_eq!(&first.payload[..], b"0123");
        assert!(first.truncated);
        assert_eq!(first.original_size, 10);
        assert_ne!(first.payload.as_ptr(), long.as_ptr());
        let latest = s.node(id).latest.expect("latest");
        assert_eq!(latest.payload.as_ptr(), long.as_ptr(), "latest shares");
        assert!(!latest.truncated);
        // A payload at the limit is shared.
        let exact = Bytes::from_static(b"abcd");
        s.insert(incoming("t", exact.clone(), MsgProps::default()));
        let first = s.history(id).next().expect("newest");
        assert!(!first.truncated);
        assert_eq!(first.payload.as_ptr(), exact.as_ptr());
        assert_eq!(s.history(NodeId::raw(99)).count(), 0);
        audit(&s);
    }

    #[test]
    fn list_is_newest_first_and_capped() {
        let mut s = TopicStore::new(Limits {
            list_len: 5,
            ..big()
        });
        let mut ids = Vec::new();
        for i in 0..8u8 {
            stored(s.insert(msg("t", &[i])));
            ids.push(RowId(u64::from(i)));
        }
        assert_eq!(s.list_len(), 5);
        assert_eq!(payload_of(&s, 0), [7]);
        assert_eq!(payload_of(&s, 4), [3]);
        assert!(s.list_row(5).is_none());
        assert_eq!(s.list_index_of(ids[7]), Some(0));
        assert_eq!(s.list_index_of(ids[3]), Some(4));
        assert_eq!(s.list_index_of(ids[2]), None, "removed by the cap");
        assert_eq!(s.stats().list_rows, 5);
        assert_eq!(s.stats().list_drops, 0);
        // History is not capped by the list.
        let id = s.find("t").expect("t");
        assert_eq!(s.history(id).count(), 8);

        // A marker lives in the same list.
        let m = s.mark_reconnected(SystemTime::UNIX_EPOCH).expect("marker");
        assert!(matches!(s.list_row(0), Some(RowView::Reconnected { id, .. }) if id == m));
        assert_eq!(s.list_len(), 5);
        assert_eq!(s.list_index_of(m), Some(0));
        match s.list_row(1).expect("row") {
            RowView::Message {
                node,
                topic,
                direction,
                status,
                ..
            } => {
                assert_eq!(
                    (node, topic, direction),
                    (Some(id), "t", Direction::Received)
                );
                assert!(status.is_none());
            }
            other => panic!("{other:?}"),
        }
        audit(&s);
    }

    #[test]
    fn sent_rows_and_status() {
        let mut s = TopicStore::new(Limits {
            list_len: 3,
            ..big()
        });
        let t = stored(s.insert(msg("a/b", b"in")));
        let Insert::Recorded(r1) = sent(&mut s, 1, "a/b", b"out1") else {
            panic!("recorded")
        };
        let Insert::Recorded(_) = sent(&mut s, 2, "new/topic", b"out2") else {
            panic!("recorded")
        };
        // A sent message does not touch the tree, the latest value or the history.
        assert_eq!(s.find("new/topic"), None);
        assert_eq!(s.stats().topics, 2);
        assert_eq!(s.stats().messages, 1);
        assert_eq!(s.history(t).count(), 1);
        assert_eq!(&s.node(t).latest.expect("latest").payload[..], b"in");

        match s.list_row(1).expect("row") {
            RowView::Message {
                id,
                node,
                topic,
                direction,
                status,
                msg,
            } => {
                assert_eq!((id, node, direction), (r1, Some(t), Direction::Sent));
                assert_eq!(status, Some(&SendStatus::Sending));
                assert_eq!(topic.as_ptr(), s.node(t).topic.as_ptr(), "topic is shared");
                assert_eq!(msg.qos, Qos::AtLeastOnce);
            }
            other => panic!("{other:?}"),
        }
        match s.list_row(0).expect("row") {
            RowView::Message { node, topic, .. } => assert_eq!((node, topic), (None, "new/topic")),
            other => panic!("{other:?}"),
        }
        let status_of = |s: &TopicStore, i| match s.list_row(i).expect("row") {
            RowView::Message { status, .. } => status.cloned(),
            other => panic!("{other:?}"),
        };
        assert!(s.set_send_status(2, SendStatus::Acknowledged));
        assert_eq!(status_of(&s, 0), Some(SendStatus::Acknowledged));
        assert!(s.set_send_status(1, SendStatus::Rejected("é".repeat(400))));
        match status_of(&s, 1) {
            Some(SendStatus::Rejected(t)) => assert!(t.len() <= STATUS_MAX && t.starts_with('é')),
            other => panic!("{other:?}"),
        }
        assert!(!s.set_send_status(99, SendStatus::Sent), "unknown op");
        audit(&s);

        // The cap removes the oldest row (the received one), then the first sent row. The
        // op map follows.
        sent(&mut s, 3, "x", b"");
        assert_eq!(s.list_len(), 3);
        assert!(s.set_send_status(1, SendStatus::TimedOut));
        sent(&mut s, 4, "x", b"");
        assert!(
            !s.set_send_status(1, SendStatus::LostOnDisconnect),
            "row removed"
        );
        assert_eq!(s.list_index_of(r1), None);
        assert!(s.set_send_status(2, SendStatus::Sent));
        assert_eq!(s.ops.len(), 3);
        // A repeated op_id points to the newest row.
        sent(&mut s, 3, "x", b"");
        assert_eq!(s.ops.get(&3), Some(&RowId(s.next_row - 1)));
        audit(&s);

        // Rows and the op map go with the memory budget too.
        let mut t = TopicStore::new(Limits {
            budget: 4 * (ROW_OVERHEAD + SENT_EXTRA + 3 + ARC_OVERHEAD),
            ..big()
        });
        for op in 0..20 {
            assert!(matches!(sent(&mut t, op, "abc", b""), Insert::Recorded(_)));
            settle(&mut t);
            audit(&t);
        }
        let rows = t.stats().list_rows;
        assert!((3..=4).contains(&rows), "{rows}");
        assert!(t.stats().list_drops >= 16);
        assert_eq!(t.ops.len(), rows);
        // A sent message that cannot fit is dropped.
        let huge = vec![0u8; t.limits.budget];
        assert_eq!(sent(&mut t, 99, "abc", &huge), Insert::Dropped);
        assert_eq!(t.stats().drops, 1);
    }

    #[test]
    fn list_filter_matching() {
        let mut s = TopicStore::new(big());
        let mut late = vec![b'x'; 1100];
        late.extend_from_slice(b"NEEDLE");
        let mut early = vec![b'x'; 1000];
        early.extend_from_slice(b"needle");
        s.insert(msg("factory/line1/temp", b"{\"V\":21}"));
        s.insert(msg("home/door", b"closed"));
        s.insert(msg("$SYS/broker", b"up"));
        s.insert(msg("n/late", &late));
        s.insert(msg("n/early", &early));
        s.insert(msg("n/unicode", "Ünï".as_bytes()));
        s.mark_reconnected(SystemTime::UNIX_EPOCH);
        sent(&mut s, 1, "factory/cmd", b"stop");
        let count = |s: &mut TopicStore, f: ListFilter| {
            s.set_list_filter(f);
            drain(s);
            s.list_len()
        };
        let text = |t: &str| ListFilter {
            text: t.into(),
            ..ListFilter::default()
        };
        assert_eq!(count(&mut s, ListFilter::default()), 8);
        assert_eq!(
            count(&mut s, text("FACTORY")),
            3,
            "topic, case-insensitive, marker"
        );
        assert_eq!(
            count(&mut s, text("\"v\"")),
            2,
            "payload text, and the marker"
        );
        assert_eq!(
            count(&mut s, text("needle")),
            2,
            "first KiB only (late is out)"
        );
        assert_eq!(count(&mut s, text("ÜNÏ")), 2, "non-ASCII is case-folded");
        assert_eq!(count(&mut s, text("zzz")), 1, "only the marker");
        let dir = |d| ListFilter {
            direction: d,
            ..ListFilter::default()
        };
        assert_eq!(count(&mut s, dir(DirectionFilter::Sent)), 2);
        assert_eq!(count(&mut s, dir(DirectionFilter::Received)), 7);
        let only = |f: &str| ListFilter {
            show_only: Some(f.into()),
            ..ListFilter::default()
        };
        assert_eq!(count(&mut s, only("factory/#")), 3);
        assert_eq!(count(&mut s, only("#")), 7, "# does not match $SYS");
        assert_eq!(count(&mut s, only("+/door")), 2);
        assert_eq!(
            count(&mut s, only("a/#/b")),
            1,
            "invalid filter matches nothing"
        );
        let g = s.filter_generation();
        s.set_list_filter(only("a/#/b"));
        assert_eq!(s.filter_generation(), g, "same filter, no change");
        audit(&s);
    }

    #[test]
    fn list_filter_is_incremental_and_equals_recompute() {
        fn run(limits: Limits, seed: u64, steps: usize) -> StoreStats {
            const TOPICS: [&str; 9] = [
                "a/b",
                "a/c",
                "x/y/z",
                "temp/room1",
                "temp/room2",
                "$SYS/up",
                "a",
                "Temp/Room3",
                "b",
            ];
            let words: [&[u8]; 5] = [b"", b"hello", b"TEMP=21", b"{\"v\":1}", "é".as_bytes()];
            let long = [vec![b'q'; 1500], b"needle".to_vec()].concat();
            let texts = ["", "a", "TEMP", "needle", "É", "zzz", "room"];
            let subs = [None, Some("a/#"), Some("+/b"), Some("temp/+"), Some("#")];
            let dirs = [
                DirectionFilter::All,
                DirectionFilter::Received,
                DirectionFilter::Sent,
            ];
            let mut s = TopicStore::new(limits);
            let mut rng = Rng(seed);
            let mut op = 0;
            for step in 0..steps {
                match rng.below(100) {
                    0..=59 => {
                        let p: &[u8] = if rng.below(9) == 0 {
                            &long
                        } else {
                            words[rng.below(words.len())]
                        };
                        s.insert(msg(TOPICS[rng.below(TOPICS.len())], p));
                    }
                    60..=66 => {
                        op += 1;
                        sent(
                            &mut s,
                            op,
                            TOPICS[rng.below(TOPICS.len())],
                            words[rng.below(5)],
                        );
                    }
                    67..=68 => {
                        s.mark_reconnected(SystemTime::UNIX_EPOCH);
                    }
                    69..=77 => s.set_list_filter(ListFilter {
                        text: texts[rng.below(texts.len())].into(),
                        direction: dirs[rng.below(3)],
                        show_only: subs[rng.below(subs.len())].map(String::from),
                    }),
                    _ => {
                        // One step with a deadline in the past: the rebuild stays unfinished.
                        s.work(Instant::now());
                    }
                }
                audit(&s);
                if step % 25 == 0 {
                    drain(&mut s);
                    assert!(!s.has_work());
                    let want = naive_ids(&s, &s.lf.filter.clone());
                    assert_eq!(view_ids(&s), want, "step {step}");
                    for (i, id) in want.iter().enumerate() {
                        assert_eq!(s.list_index_of(RowId(*id)), Some(i));
                    }
                }
            }
            s.stats()
        }
        let roomy = Limits {
            list_len: 300,
            history_len: 5,
            ..big()
        };
        let st = run(roomy, 0x9E37_79B9_7F4A_7C15, 4000);
        assert_eq!(st.list_rows, 300);
        // Tight memory: rows leave because of the budget.
        let tight = Limits {
            list_len: 100,
            history_len: 4,
            budget: 40_000,
            node_budget: 20_000,
            ..big()
        };
        let st = run(tight, 0x1234_5678_9ABC_DEF1, 4000);
        assert!(st.list_drops > 0);
    }

    #[test]
    fn a_rebuild_with_a_past_deadline_converges() {
        let mut s = TopicStore::new(big());
        for i in 0..3000 {
            s.insert(msg(&format!("t/{}", i % 40), format!("v{i}").as_bytes()));
        }
        s.set_list_filter(ListFilter {
            text: "v29".into(),
            ..ListFilter::default()
        });
        assert!(s.has_work());
        let before = s.list_len();
        assert!(s.work(Instant::now()), "one step is not enough");
        assert!(s.list_len() >= before, "the view grows");
        // Rows arrive and leave while the rebuild runs.
        for i in 0..200 {
            s.insert(msg("t/new", format!("v29{i}").as_bytes()));
            s.work(Instant::now());
        }
        let calls = drain(&mut s);
        assert!(calls >= 1);
        assert_eq!(view_ids(&s), naive_ids(&s, &s.lf.filter.clone()));
        audit(&s);
        // A generous deadline finishes in one call.
        s.set_list_filter(ListFilter {
            text: "v1".into(),
            ..ListFilter::default()
        });
        assert!(!s.work(Instant::now() + Duration::from_secs(30)));
        assert_eq!(view_ids(&s), naive_ids(&s, &s.lf.filter.clone()));
    }

    #[test]
    fn tree_filter_keeps_ancestors_and_opens_them() {
        let mut s = TopicStore::new(big());
        for t in [
            "factory/line1/temp",
            "factory/line1/speed",
            "factory/line2/temp",
            "home/garage/door",
            "home/temp",
            "other",
        ] {
            s.insert(msg(t, b"1"));
        }
        s.set_tree_filter(TreeFilter {
            text: "TEMP".into(),
            ..TreeFilter::default()
        });
        drain(&mut s);
        assert_eq!(
            row_paths(&mut s),
            [
                "factory",
                "factory/line1",
                "factory/line1/temp",
                "factory/line2",
                "factory/line2/temp",
                "home",
                "home/temp"
            ]
        );
        let f = s.find("factory").expect("factory");
        let l1t = s.find("factory/line1/temp").expect("temp");
        assert!(s.node(f).expanded && s.node(f).has_children);
        assert!(!s.node(l1t).expanded && !s.node(l1t).has_children);
        // New nodes: a match shows at once, others do not.
        s.insert(msg("home/garage/temperature", b"1"));
        s.insert(msg("home/garage/light", b"1"));
        assert!(row_paths(&mut s).contains(&"home/garage/temperature".to_string()));
        assert!(!row_paths(&mut s).contains(&"home/garage/light".to_string()));
        assert!(!row_paths(&mut s).contains(&"home/garage/door".to_string()));
        audit(&s);

        // Show only: a subscription match. `factory/#` matches the whole subtree.
        s.set_tree_filter(TreeFilter {
            text: String::new(),
            show_only: Some("factory/+/temp".into()),
        });
        drain(&mut s);
        assert_eq!(
            row_paths(&mut s),
            [
                "factory",
                "factory/line1",
                "factory/line1/temp",
                "factory/line2",
                "factory/line2/temp",
            ]
        );
        // Both filters together.
        s.set_tree_filter(TreeFilter {
            text: "line2".into(),
            show_only: Some("factory/#".into()),
        });
        drain(&mut s);
        assert_eq!(
            row_paths(&mut s),
            ["factory", "factory/line2", "factory/line2/temp"]
        );
        audit(&s);

        // No filter: the expansion flags rule again.
        s.set_tree_filter(TreeFilter::default());
        assert!(!s.has_work());
        assert_eq!(row_paths(&mut s), ["factory", "home", "other"]);
        assert!(!s.node(f).expanded);
        audit(&s);
    }

    #[test]
    fn tree_filter_rebuild_is_resumable() {
        let mut s = TopicStore::new(big());
        for i in 0..700 {
            s.insert(msg(&format!("g{}/n{}", i % 7, i), b"1"));
        }
        s.set_tree_filter(TreeFilter {
            text: "n6".into(),
            ..TreeFilter::default()
        });
        assert!(s.work(Instant::now()), "700 nodes need more than one step");
        let partial = s.rows().len();
        // A node that arrives during the rebuild is covered.
        s.insert(msg("g1/n6-late", b"1"));
        s.insert(msg("g1/other", b"1"));
        audit(&s);
        drain(&mut s);
        assert!(s.rows().len() > partial);
        let paths = row_paths(&mut s);
        assert!(paths.contains(&"g1/n6-late".to_string()));
        assert!(!paths.contains(&"g1/other".to_string()));
        audit(&s);
        // A filter change restarts the rebuild.
        s.set_tree_filter(TreeFilter {
            text: "other".into(),
            ..TreeFilter::default()
        });
        s.work(Instant::now());
        s.set_tree_filter(TreeFilter {
            text: "n69".into(),
            ..TreeFilter::default()
        });
        drain(&mut s);
        audit(&s);
        assert!(
            row_paths(&mut s)
                .iter()
                .all(|p| p.contains("n69") || !p.contains('/'))
        );
    }

    #[test]
    fn pause_freezes_the_list() {
        let mut s = TopicStore::new(big());
        for i in 0..20u8 {
            s.insert(msg(&format!("t/{}", i % 3), &[i]));
        }
        let before = view_ids(&s);
        s.set_paused(true);
        assert!(s.is_paused());
        assert_eq!(view_ids(&s), before);
        assert_eq!(s.paused_new(), 0);
        for i in 20..30u8 {
            s.insert(msg("t/0", &[i]));
        }
        sent(&mut s, 1, "t/0", b"s");
        sent(&mut s, 2, "u", b"s");
        s.mark_reconnected(SystemTime::UNIX_EPOCH);
        assert!(matches!(s.insert(msg("deep", b"d")), Insert::Stored(_)));
        assert_eq!(view_ids(&s), before, "frozen while rows arrive");
        assert_eq!(s.list_len(), 20);
        assert_eq!(s.paused_new(), 10 + 2 + 1, "markers are not counted");
        for (i, id) in before.iter().enumerate() {
            assert_eq!(s.list_index_of(RowId(*id)), Some(i));
        }
        assert_eq!(payload_of(&s, 0), [19]);
        // A status change shows in the frozen list.
        assert!(s.set_send_status(1, SendStatus::Sent));
        audit(&s);
        s.set_paused(false);
        assert_eq!(s.list_len(), 20 + 10 + 2 + 1 + 1);
        assert!(matches!(s.list_row(1), Some(RowView::Reconnected { .. })));
        assert_eq!(s.stats().list_rows, s.list_len());
        audit(&s);
    }

    #[test]
    fn pause_shows_removed_rows_and_one_marker() {
        let n = root_cost("a");
        let mut s = TopicStore::new(Limits {
            budget: n + 3 * per_msg(200),
            node_budget: 4 * n,
            ..big()
        });
        for i in 0..3u8 {
            stored(s.insert(msg("a", &[i; 200])));
            settle(&mut s);
        }
        let frozen = view_ids(&s);
        assert_eq!(frozen.len(), 3);
        s.set_paused(true);
        drain(&mut s);
        let mut dropped = 0;
        for i in 0..6u8 {
            if s.insert(msg("a", &[10 + i; 200])) == Insert::Dropped {
                dropped += 1;
            }
            settle(&mut s);
            audit(&s);
        }
        assert_eq!(dropped, 0);
        assert_eq!(s.paused_new(), 6, "removed messages still count");
        assert!(s.stats().list_drops > 0);
        // Layout: present rows, one marker, then the removed rows (newest removed first).
        let ids = view_ids(&s);
        let marker = ids.iter().position(|id| *id == u64::MAX).expect("marker");
        assert_eq!(ids.iter().filter(|id| **id == u64::MAX).count(), 1);
        assert_eq!(
            ids.len(),
            frozen.len() + 1,
            "the frozen rows keep their place"
        );
        assert_eq!(
            &ids[..marker],
            &frozen[..marker],
            "present rows did not move"
        );
        assert_eq!(
            &ids[marker + 1..],
            &frozen[marker..],
            "removed rows keep their ids"
        );
        for (i, id) in ids.iter().enumerate() {
            if *id != u64::MAX {
                assert_eq!(s.list_index_of(RowId(*id)), Some(i));
            }
        }
        assert!(matches!(s.list_row(marker), Some(RowView::RemovedMarker)));
        if let Some(RowView::Removed { .. }) = s.list_row(ids.len() - 1) {
        } else {
            panic!("the oldest frozen row is removed");
        }
        s.set_paused(false);
        assert_eq!(view_ids(&s).len(), s.stats().list_rows);
        assert!(!view_ids(&s).contains(&u64::MAX));
        audit(&s);
    }

    #[test]
    fn paused_new_counts_only_matching_rows() {
        let mut s = TopicStore::new(big());
        s.insert(msg("a/1", b"x"));
        s.insert(msg("b/1", b"x"));
        s.set_list_filter(ListFilter {
            show_only: Some("a/#".into()),
            ..ListFilter::default()
        });
        drain(&mut s);
        s.set_paused(true);
        s.insert(msg("a/2", b"x"));
        s.insert(msg("b/2", b"x"));
        sent(&mut s, 1, "a/3", b"x");
        sent(&mut s, 2, "b/3", b"x");
        assert_eq!(s.paused_new(), 2);
        assert_eq!(s.list_len(), 1);
        // A filter change starts again from now. The view takes the rows that exist now.
        s.set_list_filter(ListFilter {
            show_only: Some("b/#".into()),
            ..ListFilter::default()
        });
        assert_eq!(s.paused_new(), 0);
        drain(&mut s);
        assert_eq!(s.list_len(), 3, "b/1, b/2, b/3");
        s.insert(msg("b/4", b"x"));
        s.insert(msg("a/4", b"x"));
        assert_eq!((s.paused_new(), s.list_len()), (1, 3));
        // The same filter again changes nothing.
        s.set_list_filter(ListFilter {
            show_only: Some("b/#".into()),
            ..ListFilter::default()
        });
        assert_eq!(s.paused_new(), 1);
        // A tree filter change restarts too.
        s.set_tree_filter(TreeFilter {
            text: "b".into(),
            ..TreeFilter::default()
        });
        assert_eq!(s.paused_new(), 0);
        audit(&s);
        s.set_paused(false);
        s.set_paused(true);
        assert_eq!(s.paused_new(), 0, "a new pause starts at 0");
        audit(&s);
    }

    #[test]
    fn pause_snapshot_of_the_tree() {
        let mut s = TopicStore::new(big());
        for t in ["a/b", "a/c", "z"] {
            s.insert(msg(t, b"first value"));
        }
        let a = s.find("a").expect("a");
        s.set_expanded(a, true);
        let live = row_paths(&mut s);
        assert_eq!(live, ["a", "a/b", "a/c", "z"]);
        let used = s.stats().used_bytes;
        s.set_paused(true);
        assert!(s.has_work(), "pause only starts the snapshot");
        assert_eq!(row_paths(&mut s), live, "until then the tree is live");
        drain(&mut s);
        assert!(s.stats().used_bytes > used, "the snapshot is charged");
        let ab = s.find("a/b").expect("a/b");
        s.insert(msg("a/b", b"second"));
        s.insert(msg("a/new", b"x"));
        s.insert(msg("q", b"x"));
        assert_eq!(row_paths(&mut s), live, "rows are frozen");
        let n = s.node(ab);
        assert_eq!((n.msg_count, n.preview), (1, Some("first value")));
        assert_eq!(
            &n.latest.expect("latest").payload[..],
            b"second",
            "latest is live"
        );
        assert_eq!(s.node(a).subtree_msgs, 2);
        audit(&s);
        // The user expands a node: the snapshot is taken again, with new counts.
        let z = s.find("z").expect("z");
        s.insert(msg("z/deep", b"x"));
        s.set_expanded(z, true);
        drain(&mut s);
        assert_eq!(
            row_paths(&mut s),
            ["a", "a/b", "a/c", "a/new", "q", "z", "z/deep"]
        );
        assert_eq!(s.node(ab).msg_count, 2);
        audit(&s);
        s.set_paused(false);
        assert_eq!(s.stats().used_bytes, {
            let _ = s.rows();
            s.stats().used_bytes
        });
        assert!(s.node(ab).preview.is_none());
        assert_eq!(s.node(ab).msg_count, 2);
        assert_eq!(s.snap_bytes, 0);
        audit(&s);
    }

    #[test]
    fn snapshot_is_bounded_by_the_budget() {
        // Many visible rows and a small budget: the snapshot has fewer rows. It never exceeds
        // the budget.
        let mut s = TopicStore::new(Limits {
            budget: 400_000,
            node_budget: 300_000,
            ..big()
        });
        for i in 0..600 {
            s.insert(msg(&format!("t{i:04}"), b"x"));
            audit(&s);
        }
        s.set_paused(true);
        drain(&mut s);
        audit(&s);
        assert!(s.rows().len() <= 600);
        assert!(s.stats().used_bytes <= 400_000);
    }

    #[test]
    fn subscription_colours() {
        let mut s = TopicStore::new(big());
        let t = stored(s.insert(msg("factory/line1/temp", b"1")));
        let sys = stored(s.insert(msg("$SYS/up", b"1")));
        let other = stored(s.insert(msg("home/door", b"1")));
        assert_eq!(s.colour_of(t), None, "no subscriptions");
        s.set_subscriptions(vec!["secret/#".into(), "factory/#".into(), "#".into()]);
        assert_eq!(s.colour_of(t), Some(1), "first match wins");
        assert_eq!(s.colour_of(t), Some(1), "cached");
        assert_eq!(s.colour_of(other), Some(2));
        assert_eq!(s.colour_of(sys), None, "# does not match $SYS");
        assert_eq!(s.colour_of(NodeId::raw(999)), None);
        s.set_subscriptions(vec!["#".into(), "factory/#".into(), "$SYS/#".into()]);
        assert_eq!(s.colour_of(t), Some(0));
        assert_eq!(s.colour_of(sys), Some(2));
        assert_eq!(s.colour_of_topic("x/y"), Some(0));
        assert_eq!(s.colour_of_topic("$x"), None);
        s.set_subscriptions(vec!["a/b#".into()]);
        assert_eq!(s.colour_of(t), None, "an invalid filter matches nothing");
    }

    #[test]
    fn colours_equal_brute_force_on_random_filters() {
        let words = ["a", "b", "c", "", "$SYS", "+", "#"];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let level = |rng: &mut Rng, wild: bool| {
            let n = if wild { words.len() } else { 5 };
            words[rng.below(n)]
        };
        let mut s = TopicStore::new(big());
        let mut topics = Vec::new();
        for _ in 0..300 {
            let depth = 1 + rng.below(4);
            let t: Vec<_> = (0..depth).map(|_| level(&mut rng, false)).collect();
            topics.push(t.join("/"));
        }
        let ids: Vec<_> = topics
            .iter()
            .map(|t| stored(s.insert(msg(t, b"x"))))
            .collect();
        let mut some = 0;
        for round in 0..40 {
            let filters: Vec<String> = (0..1 + rng.below(30))
                .map(|_| {
                    let depth = 1 + rng.below(4);
                    let f: Vec<_> = (0..depth).map(|_| level(&mut rng, true)).collect();
                    let f = f.join("/");
                    match rng.below(6) {
                        0 => format!("$share/g/{f}"),
                        1 => format!("{f}+"),
                        _ => f,
                    }
                })
                .collect();
            s.set_subscriptions(filters.clone());
            for (id, t) in ids.iter().zip(&topics) {
                let want = filters.iter().position(|f| topic::matches(f, t));
                assert_eq!(s.colour_of(*id), want, "round {round}: {t} in {filters:?}");
                assert_eq!(s.colour_of_topic(t), want);
                some += usize::from(want.is_some());
            }
        }
        assert!(some > 500, "weak test data: {some}");
        // The same list again keeps the cache (no new revision).
        let rev = s.sub_rev;
        s.set_subscriptions(s.subs.raw.clone());
        assert_eq!(s.sub_rev, rev);
    }

    /// Random inserts, expands and collapses, with and without a tree filter. The spliced rows
    /// must equal a full rebuild (`audit` checks it too).
    #[test]
    fn spliced_rows_equal_a_full_rebuild() {
        let words = ["a", "b", "c", "", "B", "ab", "z", "$SYS"];
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        let mut s = TopicStore::new(big());
        for step in 0..4_000 {
            match rng.below(10) {
                0..=4 => {
                    let depth = 1 + rng.below(5);
                    let t: Vec<_> = (0..depth).map(|_| words[rng.below(words.len())]).collect();
                    s.insert(msg(&t.join("/"), b"x"));
                }
                5..=8 if s.stats().topics > 0 => {
                    let id = NodeId::raw(rng.below(s.stats().topics) as u32);
                    let open = rng.below(3) != 0;
                    s.set_expanded(id, open);
                }
                _ if step % 7 == 0 => {
                    let text = ["", "a", "b/c", "z"][rng.below(4)];
                    s.set_tree_filter(TreeFilter {
                        text: text.into(),
                        show_only: None,
                    });
                    while s.work(Instant::now() + Duration::from_millis(50)) {}
                }
                _ => {}
            }
            let expect = s.rebuild_rows();
            let live = s.live_rows().to_vec();
            assert_eq!(live, expect, "step {step}");
        }
        audit(&s);
    }

    #[test]
    fn rows_splice_is_fast_with_100k_visible_nodes() {
        let mut s = TopicStore::new(many_nodes());
        for r in 0..10u32 {
            let id = stored(s.insert(msg(&format!("r{r}/x"), b"1")));
            s.set_expanded(s.parent(id).expect("root"), true);
        }
        let t0 = Instant::now();
        let mut worst = Duration::ZERO;
        for i in 0..100_000u32 {
            // Shuffled order, so the inserts land all over the rows.
            let k = i.wrapping_mul(2_654_435_761) % 100_000;
            s.insert(msg(&format!("r{}/{k:06}_{i}", k % 10), b"1"));
            if i % 16 == 0 {
                let a = Instant::now();
                std::hint::black_box(s.rows().len());
                worst = worst.max(a.elapsed());
            }
        }
        let total = t0.elapsed();
        assert!(s.rows().len() > 90_000, "{}", s.rows().len());
        let full = s.rebuild_rows();
        assert_eq!(s.rows(), &full[..]);
        // An expand or collapse of a big node splices once.
        let r0 = s.find("r0").expect("root");
        let a = Instant::now();
        s.set_expanded(r0, false);
        s.set_expanded(r0, true);
        let toggle = a.elapsed();
        let full = s.rebuild_rows();
        assert_eq!(s.rows(), &full[..]);
        eprintln!("rows splice: 100k inserts {total:?}, worst rows() {worst:?}, toggle {toggle:?}");
        // The old code rebuilt all rows each time (about 10 ms per call at this size).
        assert!(worst < Duration::from_millis(5), "worst rows() {worst:?}");
    }

    #[test]
    fn take_returns_the_old_store() {
        fn is_send<T: Send>() {}
        is_send::<TopicStore>();
        let mut s = TopicStore::new(big());
        s.set_subscriptions(vec!["a/#".into()]);
        let id = stored(s.insert(msg("a/b", b"1")));
        let old = s.take();
        assert_eq!(old.stats().topics, 2);
        assert_eq!(old.node(id).msg_count, 1);
        assert_eq!(s.stats().topics, 0);
        let again = stored(s.insert(msg("a/b", b"1")));
        assert_eq!(s.colour_of(again), Some(0), "subscriptions stay");
        audit(&s);
    }

    /// A long run with ever-new topics and small limits. The memory never goes above the hard
    /// ceiling, `work` brings it back to the budget, new topics keep coming in, and old idle
    /// leaves go away.
    fn long_run(messages: u64, topics: u64, tree_filter: bool) {
        let limits = Limits {
            latest_payload_max: 1024,
            topic_cap: 20_000,
            budget: 9 * 512 * 1024,
            node_budget: 3 * 1024 * 1024,
            history_len: 4,
            history_payload_max: 256,
            list_len: 1_000,
        };
        let topic = |k: u64| format!("site{}/line{}/dev{k}", k % 50, (k / 50) % 100);
        let mut s = TopicStore::new(limits);
        if tree_filter {
            s.set_tree_filter(TreeFilter {
                text: "dev7".into(),
                show_only: None,
            });
        }
        let first = stored(s.insert(msg(&topic(0), b"first")));
        let payload = [7u8; 48];
        let (mut worst_work, mut work_time, mut max_used, mut max_nodes) =
            (Duration::ZERO, Duration::ZERO, 0, 0);
        let t0 = Instant::now();
        for i in 1..messages {
            s.insert(msg(&topic(i % topics), &payload));
            max_used = max_used.max(s.stats().used_bytes);
            assert!(max_used <= s.hard_limit(), "over the ceiling at {i}");
            if i % 100 == 0 {
                // One frame: the work, then the view.
                let a = Instant::now();
                while s.work(a + Duration::from_secs(10)) {}
                let took = a.elapsed();
                worst_work = worst_work.max(took);
                work_time += took;
                let st = s.stats();
                assert!(
                    st.used_bytes <= limits.budget,
                    "above the budget after work at {i}"
                );
                max_nodes = max_nodes.max(st.topics);
                if i % 20_000 == 0 {
                    if let Some(id) = s.find(&format!("site{}", i / 20_000 % 50)) {
                        s.set_expanded(id, true);
                    }
                    let n = s.rows().len();
                    assert!(tree_filter || n > 0);
                }
            }
            if i % 100_000 == 0 {
                settle(&mut s);
                audit(&s);
            }
        }
        let total = t0.elapsed();
        settle(&mut s);
        audit(&s);
        let st = s.stats();
        assert_eq!(st.topic_drops, 0, "new topics are always accepted");
        assert_eq!(st.drops, 0);
        assert!(max_nodes <= limits.topic_cap);
        assert!(s.find(&topic(0)).is_none(), "an old idle leaf is gone");
        assert!(s.find(&topic((messages - 1) % topics)).is_some());
        // The stale id is safe, also if its slot has a new node now.
        assert!(s.get(first).is_none());
        assert_eq!(s.path(first), "");
        assert_eq!(s.parent(first), None);
        assert_eq!(s.history(first).count(), 0);
        assert_eq!(s.colour_of(first), None);
        assert!(!s.has_retained(first));
        eprintln!(
            "long run: {messages} messages, {topics} topics: {:?} total ({} ns per insert incl. work), \
             work {work_time:?} (worst call {worst_work:?}), max used {} KiB (budget {} KiB, \
             ceiling {} KiB), nodes at the end {}, max {max_nodes}",
            total,
            total.as_nanos() as u64 / messages,
            max_used / 1024,
            limits.budget / 1024,
            s.hard_limit() / 1024,
            st.topics
        );
    }

    #[test]
    fn long_run_with_ever_new_topics() {
        long_run(200_000, 100_000, false);
    }

    #[test]
    fn long_run_with_a_tree_filter() {
        long_run(100_000, 50_000, true);
    }

    /// The full run: 2 million messages over 1 million topics. Run it with
    /// `cargo test --release -p navigator-core --lib long_run_two_million -- --ignored --nocapture`.
    #[test]
    #[ignore = "slow in a debug build"]
    fn long_run_two_million_messages() {
        long_run(2_000_000, 1_000_000, false);
    }

    #[test]
    fn idle_leaves_go_in_order_and_parents_follow() {
        let limits = Limits {
            topic_cap: 20,
            budget: 64 * 1024 * 1024,
            node_budget: 64 * 1024 * 1024,
            ..big()
        };
        let mut s = TopicStore::new(limits);
        // A tree filter is on: the flags of the ancestors must follow the removals (`audit`).
        s.set_tree_filter(TreeFilter {
            text: "leaf".into(),
            show_only: None,
        });
        let keep = stored(s.insert(msg("keep/x", b"1")));
        s.set_expanded(s.find("keep").expect("root"), true);
        let parent_msg = stored(s.insert(msg("p", b"own message")));
        for i in 0..30 {
            // Each topic has a pure structure parent "g{i}" that must go with its leaf.
            s.insert(msg(&format!("g{i}/leaf"), b"1"));
            // The oldest leaf is "keep/x". Touch it, so it is the newest.
            s.insert(msg("keep/x", b"2"));
            // "p" gets a child, and loses it again: it is a leaf with its own message.
            if i == 5 {
                s.insert(msg("p/child", b"c"));
            }
            settle(&mut s);
            assert!(s.stats().topics <= 20);
            audit(&s);
        }
        assert_eq!(s.stats().topic_drops, 0);
        assert!(s.get(keep).is_some(), "a recently used leaf stays");
        assert!(
            s.find("g0").is_none() && s.find("g0/leaf").is_none(),
            "no empty parent stays"
        );
        assert!(s.find("g29/leaf").is_some());
        assert!(s.get(parent_msg).is_none() || s.find("p/child").is_none());
        // The list rows of a removed node keep their topic and lose the node.
        let n = s.list_len();
        let mut no_node = 0;
        for i in 0..n {
            if let Some(RowView::Message { node, topic, .. }) = s.list_row(i)
                && topic.starts_with("g0/")
            {
                assert_eq!(node, None);
                no_node += 1;
            }
        }
        assert_eq!(no_node, 1);
        // Counts of the ancestors follow.
        assert_eq!(s.node(s.find("keep").expect("keep")).subtree_topics, 1);
        // A leaf that is open is not removed.
        s.set_expanded(s.find("g29/leaf").expect("leaf"), true);
        for i in 30..60 {
            s.insert(msg(&format!("g{i}/leaf"), b"1"));
            settle(&mut s);
        }
        assert!(s.find("g29/leaf").is_some(), "an open node stays");
        audit(&s);
    }

    #[test]
    fn nodes_are_not_removed_while_paused() {
        let limits = Limits {
            topic_cap: 20,
            ..big()
        };
        let mut s = TopicStore::new(limits);
        s.set_paused(true);
        for i in 0..40 {
            s.insert(msg(&format!("t/{i}"), b"1"));
            settle(&mut s);
        }
        assert_eq!(
            s.stats().topics,
            20,
            "full: no removal, new topics are dropped"
        );
        assert!(s.stats().topic_drops > 0);
        s.set_paused(false);
        settle(&mut s);
        assert!(s.stats().topics <= 18);
        s.insert(msg("t/new", b"1"));
        audit(&s);
    }

    #[test]
    fn budget_limits_and_live_changes() {
        assert_eq!(Limits::default(), Limits::with_budget(BUDGET_DEFAULT));
        assert_eq!(Limits::default().budget, 128 * 1024 * 1024);
        assert_eq!(Limits::with_budget(0).budget, BUDGET_MIN);
        assert_eq!(Limits::with_budget(usize::MAX).budget, BUDGET_MAX);
        assert_eq!(BUDGET_MAX, 2_147_483_648);
        let l = Limits::with_budget(512 * 1024 * 1024);
        assert_eq!((l.budget, l.node_budget), (512 << 20, 128 << 20));
        assert_eq!(l.list_len, Limits::default().list_len);

        let mut s = TopicStore::new(Limits::default());
        let payload = Bytes::from(vec![5u8; 100 * 1024]);
        for i in 0..480 {
            let m = incoming(&format!("big/{i}"), payload.clone(), MsgProps::default());
            assert!(matches!(s.insert(m), Insert::Stored(_)));
        }
        settle(&mut s);
        let before = s.stats();
        assert!(before.used_bytes > 100 << 20 && before.used_bytes <= s.budget());
        // Raising the budget needs no work.
        assert_eq!(s.set_budget(1 << 30), 1 << 30);
        assert!(!s.has_work());
        assert_eq!(s.stats().used_bytes, before.used_bytes);
        assert_eq!(s.hard_limit(), (1usize << 30) + (1 << 28));
        // Lowering evicts nothing at once. `work` does it, in slices.
        assert_eq!(s.set_budget(0), BUDGET_MIN, "clamped");
        assert_eq!(s.stats().used_bytes, before.used_bytes);
        assert!(s.has_work());
        let (mut calls, mut worst) = (0, Duration::ZERO);
        loop {
            let a = Instant::now();
            let more = s.work(a); // the deadline has passed: one round
            worst = worst.max(a.elapsed());
            calls += 1;
            if !more {
                break;
            }
        }
        assert!(calls > 1, "the eviction is sliced: {calls}");
        assert!(worst < Duration::from_millis(50), "{worst:?}");
        assert!(s.stats().used_bytes <= BUDGET_MIN);
        audit(&s);
    }

    #[test]
    fn accounting_under_a_mix_of_operations() {
        let limits = Limits {
            latest_payload_max: 4096,
            topic_cap: 300,
            budget: 160_000,
            node_budget: 80_000,
            history_len: 6,
            history_payload_max: 512,
            list_len: 80,
        };
        let mut s = TopicStore::new(limits);
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        let mut op = 0;
        for i in 0..6000 {
            match rng.below(100) {
                0..=64 => {
                    let t = rng.below(120);
                    let len = [0, 1, 40, 700, 5000][rng.below(5)];
                    s.insert(msg(
                        &format!("s{}/d{t}", t % 5),
                        &vec![b'a' + (t % 20) as u8; len],
                    ));
                }
                65..=74 => {
                    op += 1;
                    let len = [0, 10, 3000][rng.below(3)];
                    sent(
                        &mut s,
                        op,
                        &format!("s{}/d1", rng.below(5)),
                        &vec![b's'; len],
                    );
                    if rng.below(2) == 0 {
                        s.set_send_status(op, SendStatus::Rejected("no".into()));
                    }
                }
                75..=79 => {
                    s.mark_reconnected(SystemTime::UNIX_EPOCH);
                }
                80..=84 => s.set_paused(!s.is_paused()),
                85..=89 => s.set_list_filter(ListFilter {
                    text: ["", "a", "s1"][rng.below(3)].into(),
                    direction: [DirectionFilter::All, DirectionFilter::Sent][rng.below(2)],
                    show_only: [None, Some("s1/#".to_string())][rng.below(2)].clone(),
                }),
                90..=92 => s.set_tree_filter(TreeFilter {
                    text: ["", "d1", "s3"][rng.below(3)].into(),
                    show_only: None,
                }),
                93..=95 => {
                    s.work(Instant::now());
                }
                96..=97 => {
                    let ids = s.rows().to_vec();
                    if let Some(id) = ids.get(rng.below(ids.len().max(1))) {
                        s.set_expanded(*id, rng.below(2) == 0);
                    }
                }
                _ if i % 7 == 0 => s.clear(),
                _ => {}
            }
            assert!(
                s.stats().used_bytes <= s.hard_limit(),
                "over the ceiling at {i}"
            );
            if i % 20 == 0 {
                audit(&s);
            }
        }
        audit(&s);
        s.clear();
        assert_eq!(s.stats(), StoreStats::default());
        audit(&s);
    }

    #[test]
    fn pause_with_100k_visible_nodes_runs_in_bounded_slices() {
        let mut s = TopicStore::new(many_nodes());
        for i in 0..100_000u32 {
            stored(s.insert(msg(&format!("r/{i}"), b"x")));
        }
        let r = s.find("r").expect("r");
        s.set_expanded(r, true);
        assert_eq!(s.rows().len(), 100_001);
        s.set_paused(true);
        // The call only records the cutoff and starts the job.
        assert!(s.snap_rows.is_empty() && s.has_work());
        assert_eq!(
            s.rows().len(),
            100_001,
            "live rows until the snapshot is done"
        );
        let (mut calls, mut seen) = (0, 0);
        while s.work(Instant::now()) {
            calls += 1;
            let n = s.snap_rows.len();
            assert!(n - seen <= WORK_CHUNK, "a slice handles at most one chunk");
            seen = n;
            if calls % 10 == 0 {
                // Nodes that arrive during the job are not in the snapshot.
                s.insert(msg(&format!("late/{calls}"), b"x"));
            }
        }
        assert!(calls >= SNAPSHOT_MAX_ROWS / WORK_CHUNK, "{calls} slices");
        assert_eq!(s.rows().len(), SNAPSHOT_MAX_ROWS, "the cap stays");
        let rows = s.rows().to_vec();
        assert!(rows.iter().all(|id| !s.path(*id).starts_with("late")));
        assert_eq!(s.node(rows[5]).preview, Some("x"));
        audit(&s);
        s.set_paused(false);
        drain(&mut s); // the list is above its live cap
        assert!(s.snap_bytes == 0 && s.snap_job.is_none());
        audit(&s);
    }

    #[test]
    fn a_snapshot_restart_and_unpause_stop_the_job() {
        let mut s = TopicStore::new(big());
        for i in 0..1000 {
            s.insert(msg(&format!("t{i}"), b"x"));
        }
        s.set_paused(true);
        assert!(s.work(Instant::now()));
        s.set_paused(false);
        assert!(!s.has_work());
        s.set_paused(true);
        s.insert(msg("new", b"x"));
        drain(&mut s);
        assert_eq!(s.rows().len(), 1000, "no node from after the pause");
        audit(&s);
    }

    #[test]
    fn text_filter_uses_unicode_lowercase_on_every_path() {
        // The Kelvin sign lowercases to ASCII k. U+0130 lowercases to i and a combining dot.
        let mut s = TopicStore::new(big());
        s.insert(msg("a/\u{212A}", b"1"));
        s.insert(msg("b", "\u{130}x".as_bytes()));
        s.insert(msg("c/k", b"plain"));
        s.insert(msg("d", b"none"));
        for text in ["k", "K", "i", "I", "\u{212A}", "x"] {
            let f = ListFilter {
                text: text.into(),
                ..ListFilter::default()
            };
            // Full recompute.
            s.set_list_filter(f.clone());
            drain(&mut s);
            assert_eq!(view_ids(&s), naive_ids(&s, &f), "recompute {text:?}");
            // Incremental path: the new rows match the same way.
            s.insert(msg("e/\u{212A}", b"2"));
            s.insert(msg("f", "\u{130}".as_bytes()));
            assert_eq!(view_ids(&s), naive_ids(&s, &f), "incremental {text:?}");
            let t = TreeFilter {
                text: text.into(),
                ..TreeFilter::default()
            };
            s.set_tree_filter(t.clone());
            drain(&mut s);
            let want: Vec<String> = (0..s.nodes.len())
                .map(|i| s.nodes[i].topic.to_string())
                .filter(|p| naive_tree(&t, p))
                .collect();
            let shown = row_paths(&mut s);
            for p in &want {
                assert!(
                    shown
                        .iter()
                        .any(|x| x == p || x.len() < p.len() && p.starts_with(x.as_str())),
                    "tree {text:?} shows {p}"
                );
            }
            audit(&s);
        }
        let k = ListFilter {
            text: "k".into(),
            ..ListFilter::default()
        };
        s.set_list_filter(k);
        drain(&mut s);
        assert!(s.list_len() >= 4, "Kelvin and plain k match");
    }

    #[test]
    fn row_evicted_tells_removed_rows() {
        let mut s = TopicStore::new(Limits {
            list_len: 5,
            ..big()
        });
        for i in 0..8u8 {
            s.insert(msg("t", &[i]));
        }
        assert!(s.row_evicted(RowId(0)) && s.row_evicted(RowId(2)));
        assert!(!s.row_evicted(RowId(3)) && !s.row_evicted(RowId(7)));
        assert!(!s.row_evicted(RowId(8)), "not issued yet");
        s.set_list_filter(ListFilter {
            text: "zzz".into(),
            ..ListFilter::default()
        });
        drain(&mut s);
        assert_eq!(s.list_len(), 0);
        assert!(
            !s.row_evicted(RowId(5)),
            "hidden by the filter, not removed"
        );
        s.clear();
        assert!(s.row_evicted(RowId(5)));
    }

    /// 100k topics and 10k list rows. The filter rebuilds run in slices. There is no timing
    /// assert: run `cargo test --release -- --nocapture perf` to see the numbers.
    #[test]
    fn perf_sanity_with_100k_topics_and_10k_rows() {
        let mut s = TopicStore::new(many_nodes());
        let t0 = Instant::now();
        for i in 0..100_000u32 {
            let topic = format!("site{}/line{}/dev{i}", i % 10, (i / 10) % 100);
            let payload = format!("{{\"v\":{i},\"unit\":\"C\"}}");
            stored(s.insert(msg(&topic, payload.as_bytes())));
        }
        let fill = t0.elapsed();
        assert_eq!(s.stats().topics, 100_000 + 10 + 1000);
        assert_eq!(s.list_len(), 10_000);

        let slices = |s: &mut TopicStore| {
            let t = Instant::now();
            let (mut n, mut worst) = (0, Duration::ZERO);
            loop {
                let a = Instant::now();
                let more = s.work(a + Duration::from_millis(4));
                worst = worst.max(a.elapsed());
                n += 1;
                if !more {
                    break;
                }
            }
            (n, worst, t.elapsed())
        };
        s.set_list_filter(ListFilter {
            text: "dev9999".into(),
            ..ListFilter::default()
        });
        let (n, worst, total) = slices(&mut s);
        let list_text = s.list_len();
        assert_eq!(view_ids(&s), naive_ids(&s, &s.lf.filter.clone()));
        s.set_list_filter(ListFilter {
            text: "\"v\":4242".into(),
            show_only: Some("site2/+/+".into()),
            direction: DirectionFilter::Received,
        });
        let (n2, worst2, total2) = slices(&mut s);
        assert_eq!(view_ids(&s), naive_ids(&s, &s.lf.filter.clone()));
        s.set_tree_filter(TreeFilter {
            text: "line42/dev4".into(),
            ..TreeFilter::default()
        });
        let (n3, worst3, total3) = slices(&mut s);
        let t = Instant::now();
        let rows = s.rows().len();
        let rows_time = t.elapsed();
        assert!(rows > 0);
        s.set_tree_filter(TreeFilter {
            show_only: Some("site3/#".into()),
            ..TreeFilter::default()
        });
        slices(&mut s);
        let t = Instant::now();
        let rows3 = s.rows().len();
        let rows3_time = t.elapsed();
        assert_eq!(rows3, 1 + 100 + 10_000, "site3, its lines, its devices");
        let t = Instant::now();
        s.set_paused(true);
        let pause_time = t.elapsed();
        drain(&mut s);
        // The worst case: every node is visible, so the snapshot has the most rows.
        s.set_paused(false);
        s.set_tree_filter(TreeFilter {
            show_only: Some("+/#".into()),
            ..TreeFilter::default()
        });
        slices(&mut s);
        let t = Instant::now();
        s.set_paused(true);
        let pause_max_time = t.elapsed();
        drain(&mut s);
        assert_eq!(s.rows().len(), SNAPSHOT_MAX_ROWS);
        audit(&s);
        println!(
            "perf: fill {fill:?}; list text: {n} slices, worst {worst:?}, total {total:?}, {list_text} rows; \
             list 3 filters: {n2} slices, worst {worst2:?}, total {total2:?}; tree: {n3} slices, \
             worst {worst3:?}, total {total3:?}, rows() {rows_time:?} ({rows} rows); site3 rows() \
             {rows3_time:?}; pause {pause_time:?} (10k rows), {pause_max_time:?} (cap)"
        );
    }

    fn retained_msg(topic: &str, payload: &[u8], retain: bool, expiry: Option<u32>) -> Incoming {
        Incoming {
            retain,
            props: MsgProps {
                message_expiry: expiry,
                ..MsgProps::default()
            },
            ..msg(topic, payload)
        }
    }

    #[test]
    fn retained_state_follows_the_broker() {
        let mut s = TopicStore::new(big());
        let id = stored(s.insert(retained_msg("t", b"v", true, None)));
        assert!(s.has_retained(id));
        // A live (not retained) update does not change it. The latest message has no flag.
        s.insert(msg("t", b"live"));
        assert!(!s.node(id).latest.expect("latest").retain);
        assert!(s.has_retained(id));
        // A retained message with an empty payload is a delete.
        s.insert(retained_msg("t", b"", true, None));
        assert!(!s.has_retained(id));
        // An empty payload without the flag is not a delete, and it does not set the state.
        s.insert(retained_msg("t", b"x", true, None));
        s.insert(msg("t", b""));
        assert!(s.has_retained(id));
        assert!(!s.has_retained(NodeId::raw(999)));
        let other = stored(s.insert(msg("u", b"v")));
        assert!(!s.has_retained(other));
        s.clear();
        assert!(!s.has_retained(id));
    }

    #[test]
    fn retained_state_expires_on_read() {
        let mut s = TopicStore::new(big());
        let t0 = SystemTime::UNIX_EPOCH;
        let id = stored(s.insert(retained_msg("t", b"v", true, Some(10))));
        assert!(s.has_retained_at(id, t0 + Duration::from_secs(9)));
        assert!(!s.has_retained_at(id, t0 + Duration::from_secs(10)));
        // The interval belongs to the retained message: a live update does not renew it.
        s.insert(msg("t", b"live"));
        assert!(!s.has_retained_at(id, t0 + Duration::from_secs(11)));
        // A new retained message without an interval does not expire.
        s.insert(retained_msg("t", b"v2", true, None));
        assert!(s.has_retained_at(id, t0 + Duration::from_secs(1_000_000)));
    }

    #[test]
    fn our_acknowledged_clear_ends_the_retained_state() {
        let mut s = TopicStore::new(big());
        let id = stored(s.insert(retained_msg("t", b"v", true, None)));
        let clear = |s: &mut TopicStore, op, qos, retain, payload: &'static [u8]| {
            s.record_sent(
                op,
                "t",
                Bytes::from_static(payload),
                qos,
                retain,
                MsgProps::default(),
                SystemTime::UNIX_EPOCH,
            );
        };
        // Sending is not enough. A normal publish and a non-retained empty one do not clear.
        clear(&mut s, 1, Qos::AtLeastOnce, true, b"");
        assert!(s.has_retained(id));
        clear(&mut s, 2, Qos::AtLeastOnce, true, b"x");
        s.set_send_status(2, SendStatus::Acknowledged);
        clear(&mut s, 3, Qos::AtLeastOnce, false, b"");
        s.set_send_status(3, SendStatus::Acknowledged);
        s.set_send_status(1, SendStatus::Rejected("no".into()));
        assert!(s.has_retained(id));
        // A rejected delete is over: a late "acknowledged" for it changes nothing.
        s.set_send_status(1, SendStatus::Acknowledged);
        assert!(s.has_retained(id));
        // The acknowledgement of our own retained delete clears it.
        clear(&mut s, 5, Qos::AtLeastOnce, true, b"");
        s.set_send_status(5, SendStatus::Acknowledged);
        assert!(!s.has_retained(id));
        // QoS 0 has no acknowledgement: "Sent" is the end.
        s.insert(retained_msg("t", b"v", true, None));
        clear(&mut s, 4, Qos::AtMostOnce, true, b"");
        s.set_send_status(4, SendStatus::Sent);
        assert!(!s.has_retained(id));
    }

    #[test]
    fn the_clear_completes_after_its_list_row_was_evicted() {
        let mut s = TopicStore::new(Limits {
            list_len: 3,
            ..big()
        });
        let id = stored(s.insert(retained_msg("t", b"v", true, None)));
        s.record_sent(
            1,
            "t",
            Bytes::new(),
            Qos::AtLeastOnce,
            true,
            MsgProps::default(),
            SystemTime::UNIX_EPOCH,
        );
        // "Sent" is not the end of a QoS 1 delete.
        s.set_send_status(1, SendStatus::Sent);
        assert!(s.has_retained(id));
        // New rows push the sent row out of the list. Its op id is gone.
        for i in 0..10 {
            s.insert(msg(&format!("x/{i}"), b"1"));
        }
        assert!(!s.set_send_status(1, SendStatus::Acknowledged), "no row");
        assert!(!s.has_retained(id), "the late acknowledgement still clears");
        // The pending list is bounded: the oldest delete is forgotten.
        let id = stored(s.insert(retained_msg("t", b"v", true, None)));
        for op in 100..100 + MAX_PENDING_RETAINED as u64 + 1 {
            s.record_sent(
                op,
                "t",
                Bytes::new(),
                Qos::AtLeastOnce,
                true,
                MsgProps::default(),
                SystemTime::UNIX_EPOCH,
            );
        }
        assert_eq!(s.pending_retained.len(), MAX_PENDING_RETAINED);
        s.set_send_status(100, SendStatus::Acknowledged);
        assert!(s.has_retained(id), "the oldest delete was dropped");
        s.set_send_status(101, SendStatus::Acknowledged);
        assert!(!s.has_retained(id));
    }

    #[test]
    fn a_broker_retained_message_replaces_our_own_record() {
        let mut s = TopicStore::new(big());
        let t0 = SystemTime::UNIX_EPOCH;
        // We publish a retained message without expiry, and the broker acknowledges it.
        s.record_sent(
            1,
            "r/t",
            Bytes::from_static(b"ours"),
            Qos::AtLeastOnce,
            true,
            MsgProps::default(),
            t0,
        );
        s.set_send_status(1, SendStatus::Acknowledged);
        assert!(s.has_retained_topic("r/t"));
        // Then the broker sends a newer retained value that expires after 5 s.
        let m = retained_msg("r/t", b"theirs", true, Some(5));
        let at = m.received_at;
        s.insert(m);
        assert!(s.has_retained_topic_at("r/t", at + Duration::from_secs(4)));
        // When that value expires, the broker holds nothing; our older record must not count.
        assert!(!s.has_retained_topic_at("r/t", at + Duration::from_secs(6)));
    }

    #[test]
    fn our_own_retained_publish_is_known_without_a_node() {
        let mut s = TopicStore::new(big());
        let t0 = SystemTime::UNIX_EPOCH;
        let send = |s: &mut TopicStore, op, qos, payload: &'static [u8], expiry: Option<u32>| {
            s.record_sent(
                op,
                "out/t",
                Bytes::from_static(payload),
                qos,
                true,
                MsgProps {
                    message_expiry: expiry,
                    ..MsgProps::default()
                },
                t0,
            );
        };
        assert!(s.find("out/t").is_none() && !s.has_retained_topic("out/t"));
        // Not yet: the publish is only sent.
        send(&mut s, 1, Qos::AtLeastOnce, b"v", Some(10));
        s.set_send_status(1, SendStatus::Sent);
        assert!(!s.has_retained_topic_at("out/t", t0));
        s.set_send_status(1, SendStatus::Acknowledged);
        assert!(s.has_retained_topic_at("out/t", t0 + Duration::from_secs(9)));
        assert!(!s.has_retained_topic_at("out/t", t0 + Duration::from_secs(10)));
        // A rejected publish changes nothing.
        send(&mut s, 2, Qos::AtLeastOnce, b"w", None);
        s.set_send_status(2, SendStatus::Rejected("no".into()));
        assert!(!s.has_retained_topic_at("out/t", t0 + Duration::from_secs(11)));
        // QoS 0 counts at "Sent". An empty retained publish ends it.
        send(&mut s, 3, Qos::AtMostOnce, b"x", None);
        s.set_send_status(3, SendStatus::Sent);
        assert!(s.has_retained_topic("out/t"));
        send(&mut s, 4, Qos::AtLeastOnce, b"", None);
        s.set_send_status(4, SendStatus::Acknowledged);
        assert!(!s.has_retained_topic("out/t"));
        // An incoming retained delete ends it, too.
        send(&mut s, 5, Qos::AtMostOnce, b"y", None);
        s.set_send_status(5, SendStatus::Sent);
        s.insert(retained_msg("out/t", b"", true, None));
        assert!(!s.has_retained_topic("out/t"));
        // With a node, the node state counts.
        let id = stored(s.insert(retained_msg("in/t", b"v", true, None)));
        assert!(s.has_retained_topic("in/t") && s.has_retained(id));
        // The map is bounded.
        for i in 0..MAX_SENT_RETAINED + 5 {
            let op = 1000 + i as u64;
            s.record_sent(
                op,
                &format!("many/{i}"),
                Bytes::from_static(b"v"),
                Qos::AtMostOnce,
                true,
                MsgProps::default(),
                t0,
            );
            s.set_send_status(op, SendStatus::Sent);
        }
        assert_eq!(s.sent_retained.by_topic.len(), MAX_SENT_RETAINED);
        assert_eq!(s.sent_retained.order.len(), MAX_SENT_RETAINED);
        assert!(!s.has_retained_topic("many/0") && s.has_retained_topic("many/1028"));
    }
}
