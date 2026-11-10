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
//! The eviction order is: oldest history entries, oldest list rows, oldest latest payloads.
//! The Messages view has priority over history, because history grows with the topic count.
//!
//! # Views and work
//!
//! - The list view and the tree view have filters. The list filter is kept up to date on each
//!   insert. A filter change starts a rebuild. The rebuild runs in slices in [`TopicStore::work`].
//!   While it runs, the list shows the newest matches first (the rebuild scans from new to old).
//! - [`TopicStore::set_paused`] freezes both views. Ingest continues inside the budget. The
//!   snapshot of the tree is built in slices by `work`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Bound;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use bytes::Bytes;

use crate::model::{Incoming, MsgProps, Qos};
use crate::topic;

/// Most old values one insert may evict. If more space is needed, the message is dropped.
pub const MAX_EVICTIONS_PER_INSERT: usize = 1024;

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

/// Index of a node in the arena. Stable until `clear`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// Identity of one row of the message list. Never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RowId(pub u64);

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

impl Default for Limits {
    fn default() -> Self {
        Self {
            latest_payload_max: 1024 * 1024,
            topic_cap: 200_000,
            budget: 128 * 1024 * 1024,
            node_budget: 32 * 1024 * 1024,
            history_len: 100,
            history_payload_max: 64 * 1024,
            list_len: 10_000,
        }
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
}

impl ListFilterState {
    fn new(filter: ListFilter) -> Self {
        Self {
            needle: Needle::new(&filter.text),
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
    if let Some(sub) = &f.filter.show_only
        && !topic::matches(sub, &m.topic)
    {
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
    /// Arena. A `NodeId` is an index. Parents have a lower index than their children.
    nodes: Vec<Node>,
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
    subs: Vec<String>,
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
            subs: Vec::new(),
            sub_rev: 1,
        }
    }

    /// Removes all nodes, messages and rows. Counters reset too.
    ///
    /// These stay: the limits, the filters, the subscriptions and the next `RowId` (an old
    /// `RowId` never points to a new row). Pause ends.
    pub fn clear(&mut self) {
        let mut fresh = Self::new(self.limits);
        fresh.next_row = self.next_row;
        fresh.scan_below = self.next_row;
        fresh.filter_gen = self.filter_gen + 1;
        fresh.lf = std::mem::take(&mut self.lf);
        fresh.tf = std::mem::take(&mut self.tf);
        fresh.subs = std::mem::take(&mut self.subs);
        fresh.sub_rev = self.sub_rev.wrapping_add(1).max(1);
        *self = fresh;
    }

    // ---------------------------------------------------------------------------------------
    // Insert
    // ---------------------------------------------------------------------------------------

    /// Inserts one received message. Creates missing nodes. Updates counts on the path. Adds the
    /// message to the latest value, the history of the topic and the message list.
    /// Never exceeds the budgets. `msg.generation` is not checked here.
    ///
    /// The store checks the whole cost first. If the message does not fit, the store evicts
    /// (see the module docs). A drop can happen after some evictions (the evictions stay).
    /// The topic keeps its old latest value until the new message is admitted.
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
        let latest_cost = stored_len + props_cost;
        let hist_len = stored_len.min(self.limits.history_payload_max);
        let hist_cost = HIST_OVERHEAD + hist_len + props_cost;
        let row_cost = ROW_OVERHEAD + stored_len + props_cost;
        let cost = latest_cost + hist_cost + row_cost;
        if new_nodes > 0
            && (self.nodes.len() + new_nodes > self.limits.topic_cap
                || self.node_bytes + new_node_bytes > self.limits.node_budget)
        {
            self.topic_drops += 1;
            return Insert::TopicDropped;
        }
        if self.fixed() + new_node_bytes + cost > self.limits.budget {
            // Does not fit even if everything that can be evicted is evicted.
            self.drops += 1;
            return Insert::Dropped;
        }

        // Make room. The old latest value of this topic counts as free space, but it is released
        // only when the message is stored.
        let existing = if new_nodes == 0 { parent } else { None };
        let own = existing.map_or(0, |id| {
            self.nodes[id.0 as usize]
                .latest
                .as_ref()
                .map_or(0, msg_cost)
        });
        if !self.make_room(new_node_bytes + cost, own, existing) {
            self.drops += 1;
            return Insert::Dropped;
        }
        if let Some(id) = existing {
            self.release(id);
        }
        debug_assert!(self.used() + new_node_bytes + cost <= self.limits.budget);

        // Create the missing nodes.
        let parent_visible = parent.is_none_or(|p| self.nodes[p.0 as usize].expanded);
        let mut leaf = parent;
        for (i, name) in missing.enumerate() {
            let id = NodeId(self.nodes.len() as u32); // topic_cap <= u32::MAX is checked above
            let depth = leaf.map_or(0, |p| self.nodes[p.0 as usize].depth.saturating_add(1));
            let end = prefix_end(&topic, name);
            let node_topic = if end == topic.len() {
                Arc::clone(&topic)
            } else {
                Arc::from(&topic[..end])
            };
            self.nodes.push(Node {
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
            });
            self.node_bytes += node_cost(name.len(), end);
            match leaf {
                None => self.roots.insert(name.into(), id),
                Some(p) => self.nodes[p.0 as usize].children.insert(name.into(), id),
            };
            // Only the first new node can show up in the rows: the others are below a new
            // (collapsed) node.
            if i == 0 && parent_visible {
                self.rows_dirty = true;
            }
            if self.tf.active {
                self.tree_flags.push(0);
                // A node below the cursor is tested by the rebuild, a node at it is tested now.
                if self.tree_cursor == id.0 as usize {
                    self.tree_test(id);
                    self.tree_cursor += 1;
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
        let node = &mut self.nodes[id.0 as usize];
        let first = node.msg_count == 0;
        node.msg_count += 1;
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
        while node.history.len() > self.limits.history_len {
            if let Some(old) = node.history.pop_front() {
                self.hist_order.remove(&old.seq);
                self.hist_bytes -= old.charge;
            }
        }

        // Counts: this node and all ancestors.
        self.nodes[id.0 as usize].subtree_msgs += 1;
        let mut up = self.nodes[id.0 as usize].parent;
        while let Some(p) = up {
            let n = &mut self.nodes[p.0 as usize];
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
        let shared = node.map(|n| Arc::clone(&self.nodes[n.0 as usize].topic));
        let topic_cost = if shared.is_some() {
            0
        } else {
            topic.len() + ARC_OVERHEAD
        };
        let original_size = payload.len();
        let stored_len = original_size.min(self.limits.latest_payload_max);
        let cost = ROW_OVERHEAD + SENT_EXTRA + topic_cost + stored_len + props.heap_bytes();
        if self.fixed() + cost > self.limits.budget || !self.make_room(cost, 0, None) {
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
        Insert::Recorded(id)
    }

    /// Changes the status of the sent message with this `op_id`. Returns false if the row is
    /// not in the list any more (or never was).
    pub fn set_send_status(&mut self, op_id: u64, status: SendStatus) -> bool {
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
        match &mut row.kind {
            RowKind::Message(m) => {
                m.status = Some(status);
                true
            }
            RowKind::Reconnected(_) => false,
        }
    }

    /// Adds a "Reconnected" marker row to the list. The marker has a small fixed charge and it
    /// is not counted in `paused_new`. Returns `None` if even a marker does not fit.
    pub fn mark_reconnected(&mut self, at: SystemTime) -> Option<RowId> {
        if self.fixed() + ROW_OVERHEAD > self.limits.budget
            || !self.make_room(ROW_OVERHEAD, 0, None)
        {
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
            // ponytail: full rebuild, O(visible rows). Each new visible node costs one rebuild,
            // and a filter that opens 100k nodes makes each rebuild long. Make it an incremental
            // splice if a measurement shows a problem.
            let mut rows = std::mem::take(&mut self.rows);
            rows.clear();
            let filtered = self.tf.active;
            let flag = |id: NodeId| self.tree_flags.get(id.0 as usize).copied().unwrap_or(0);
            let mut stack = vec![self.roots.values()];
            while let Some(it) = stack.last_mut() {
                match it.next() {
                    Some(&id) => {
                        if filtered && flag(id) & T_VISIBLE == 0 {
                            continue;
                        }
                        rows.push(id);
                        let n = &self.nodes[id.0 as usize];
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
            self.rows = rows;
            self.rows_dirty = false;
        }
        &self.rows
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
        let n = self.nodes.get(id.0 as usize)?;
        let snap = if self.paused && self.snap_job.is_none() {
            self.snap.get(&id)
        } else {
            None
        };
        let (has_children, expanded) = if self.tf.active {
            let open = self.tree_flags.get(id.0 as usize).copied().unwrap_or(0) & T_OPEN != 0;
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
        self.nodes
            .get(id.0 as usize)
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
        self.nodes.get(id.0 as usize)?.parent
    }

    /// Expands or collapses a node. An unknown id is ignored.
    /// A tree filter ignores the flag until the filter is cleared. While paused, this starts a
    /// new snapshot (built by `work`), so the counts of the tree refresh once.
    pub fn set_expanded(&mut self, id: NodeId, expanded: bool) {
        if let Some(n) = self.nodes.get_mut(id.0 as usize)
            && n.expanded != expanded
        {
            n.expanded = expanded;
            self.rows_dirty = true;
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
        self.nodes
            .get(id.0 as usize)
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
            return self.row(self.view_id(i, present)).map(Row::view);
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
        self.scanning
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
            if self.tf.active && self.tree_cursor < self.nodes.len() {
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
    /// colours. Invalid filters match nothing.
    pub fn set_subscriptions(&mut self, filters: Vec<String>) {
        self.subs = filters;
        self.sub_rev = self.sub_rev.wrapping_add(1).max(1);
    }

    /// Index of the first subscription that matches the topic of the node. The result is cached
    /// for each node until the subscriptions change. Call it for visible rows only.
    /// An unknown node gives `None`.
    pub fn colour_of(&mut self, node: NodeId) -> Option<usize> {
        let rev = self.sub_rev;
        let n = self.nodes.get_mut(node.0 as usize)?;
        if n.colour_rev != rev {
            n.colour_rev = rev;
            n.colour_idx = self
                .subs
                .iter()
                .position(|s| topic::matches(s, &n.topic))
                .map_or(NO_COLOUR, |i| u32::try_from(i).unwrap_or(NO_COLOUR));
        }
        (n.colour_idx != NO_COLOUR).then_some(n.colour_idx as usize)
    }

    /// Like `colour_of`, for a topic without a node (a sent message). It is not cached.
    pub fn colour_of_topic(&self, topic: &str) -> Option<usize> {
        self.subs.iter().position(|s| topic::matches(s, topic))
    }

    // ---------------------------------------------------------------------------------------

    pub fn stats(&self) -> StoreStats {
        StoreStats {
            topics: self.nodes.len(),
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

    /// `None` is the virtual root.
    fn children_of(&self, id: Option<NodeId>) -> &BTreeMap<Box<str>, NodeId> {
        match id {
            None => &self.roots,
            Some(id) => &self.nodes[id.0 as usize].children,
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

    /// Evicts until `need` more bytes fit. `own_free` bytes (the old latest value of the topic
    /// that is replaced) count as free space. `keep` is that topic. Returns false if the work cap
    /// is reached or nothing is left to evict.
    fn make_room(&mut self, need: usize, own_free: usize, keep: Option<NodeId>) -> bool {
        let mut evicted = 0;
        while self.used() + need > self.limits.budget + own_free {
            if evicted == MAX_EVICTIONS_PER_INSERT || !self.evict_one(keep) {
                return false;
            }
            evicted += 1;
        }
        true
    }

    /// Evicts one item. The order: oldest history entry, oldest list row, oldest latest payload.
    fn evict_one(&mut self, keep: Option<NodeId>) -> bool {
        self.evict_history() || self.pop_row(true) || self.evict_latest(keep)
    }

    /// Removes the oldest history entry of all topics. It is the front of one ring.
    fn evict_history(&mut self) -> bool {
        let Some((seq, id)) = self.hist_order.pop_first() else {
            return false;
        };
        if let Some(e) = self.nodes[id.0 as usize].history.pop_front() {
            debug_assert_eq!(e.seq, seq);
            self.hist_bytes -= e.charge;
        }
        true
    }

    /// Takes the bytes of the latest value of a node out of the accounting.
    /// The caller replaces the value right after.
    fn release(&mut self, id: NodeId) {
        let n = &self.nodes[id.0 as usize];
        let cost = n.latest.as_ref().map_or(0, msg_cost);
        if cost > 0 {
            self.order.remove(&n.seq);
            self.latest_bytes -= cost;
        }
    }

    /// Empties the oldest non-empty latest value, except the value of `keep` (the topic that is
    /// being replaced: it is released only when its new value is stored). Returns false if there
    /// is nothing to evict.
    fn evict_latest(&mut self, keep: Option<NodeId>) -> bool {
        let Some((&seq, &id)) = self.order.iter().find(|(_, id)| Some(**id) != keep) else {
            return false;
        };
        self.order.remove(&seq);
        if let Some(m) = self.nodes[id.0 as usize].latest.as_mut() {
            self.latest_bytes -= msg_cost(m);
            m.payload = Bytes::new();
            m.props = MsgProps::default(); // ponytail: props go too, so the cost is exactly 0
            m.truncated = true;
        }
        true
    }

    /// Adds a row to the list. The charge is already admitted (`make_room`). Updates the filter
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
            self.tree_test(NodeId(i as u32));
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
        let n = &self.nodes[id.0 as usize];
        let hit = self.tf.needle.found_in(&n.topic)
            && self
                .tf
                .filter
                .show_only
                .as_ref()
                .is_none_or(|s| topic::matches(s, &n.topic));
        if !hit {
            return;
        }
        self.tree_flags[id.0 as usize] |= T_MATCH | T_VISIBLE;
        // An ancestor that is open has open ancestors, so the walk stops there.
        let mut up = n.parent;
        while let Some(p) = up {
            let f = &mut self.tree_flags[p.0 as usize];
            if *f & T_OPEN != 0 {
                break;
            }
            *f |= T_OPEN | T_VISIBLE;
            up = self.nodes[p.0 as usize].parent;
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
                .range::<str, _>((
                    Bound::Excluded(&*self.nodes[l.0 as usize].name),
                    Bound::Unbounded,
                ))
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
            let flags = self.tree_flags.get(id.0 as usize).copied().unwrap_or(0);
            let skip = id.0 as usize >= job.node_limit || (filtered && flags & T_VISIBLE == 0);
            let mut open = false;
            if !skip {
                if self.snap_rows.len() >= SNAPSHOT_MAX_ROWS {
                    done = true;
                    break;
                }
                let node = &self.nodes[id.0 as usize];
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
                    if !self.evict_one(None) {
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

/// Byte offset of the end of `part` in `topic`. `part` must be a slice of `topic`.
fn prefix_end(topic: &str, part: &str) -> usize {
    part.as_ptr() as usize - topic.as_ptr() as usize + part.len()
}

#[cfg(test)]
mod tests {
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
        for (i, n) in s.nodes.iter().enumerate() {
            nodes += node_cost(n.name.len(), n.topic.len());
            let mut names = vec![&*n.name];
            let mut p = n.parent;
            while let Some(q) = p {
                names.push(&s.nodes[q.0 as usize].name);
                p = s.nodes[q.0 as usize].parent;
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
                assert_eq!(s.hist_order.get(&e.seq), Some(&NodeId(i as u32)));
                hist += e.charge;
                hist_n += 1;
            }
        }
        assert_eq!(s.node_bytes, nodes);
        assert_eq!(s.latest_bytes, latest);
        assert_eq!(s.hist_bytes, hist);
        assert_eq!(s.hist_order.len(), hist_n);
        let live = s
            .nodes
            .iter()
            .filter(|n| n.latest.as_ref().is_some_and(|m| msg_cost(m) > 0));
        assert_eq!(s.order.len(), live.count());
        for (seq, id) in &s.order {
            assert_eq!(s.nodes[id.0 as usize].seq, *seq);
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
                            base + SENT_EXTRA
                                + if m.node.is_none() {
                                    m.topic.len() + ARC_OVERHEAD
                                } else {
                                    0
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
                if naive_tree(&s.tf.filter, &n.topic) {
                    want[i] |= T_MATCH | T_VISIBLE;
                    let mut up = n.parent;
                    while let Some(p) = up {
                        want[p.0 as usize] |= T_OPEN | T_VISIBLE;
                        up = s.nodes[p.0 as usize].parent;
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
        assert!(total <= lim.budget, "over budget");
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
        s.set_expanded(NodeId(9999), true); // unknown id is ignored
        assert_eq!(s.path(NodeId(9999)), "");
        assert_eq!(s.parent(NodeId(9999)), None);
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
                s.stats().used_bytes <= limits.budget,
                "over budget at insert {i}"
            );
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
            budget: 2 * n + per_msg(600) + 100,
            node_budget: 2 * n,
            ..big()
        };
        let mut s = TopicStore::new(limits);
        let a = stored(s.insert(msg("a", &[1; 600])));
        let b = stored(s.insert(msg("b", &[2; 600]))); // evicts the row, the history, then a
        let ma = s.node(a).latest.expect("a keeps its message record");
        assert!(ma.payload.is_empty() && ma.truncated);
        assert_eq!(ma.original_size, 600);
        assert_eq!(s.node(a).msg_count, 1);
        assert_eq!(s.find("a"), Some(a));
        assert_eq!(s.history(a).count(), 0);
        assert_eq!(s.history(b).count(), 1);
        assert_eq!(s.node(b).latest.expect("b").payload.len(), 600);
        assert_eq!(s.stats().used_bytes, 2 * n + per_msg(600));
        assert_eq!(s.stats().list_drops, 1);
        audit(&s);

        // A bigger value for b does not fit, even when b's row and history go. The old value
        // stays: it is released only when the new value is admitted.
        assert_eq!(s.insert(msg("b", &[3; 900])), Insert::Dropped);
        assert_eq!(s.node(b).latest.expect("b").payload.len(), 600);
        assert_eq!(s.stats().drops, 1);
        audit(&s);
        // A smaller value fits: the old cost counts as free space.
        stored(s.insert(msg("b", &[3; 500])));
        assert_eq!(s.node(b).latest.expect("b").payload.len(), 500);
        audit(&s);

        // Too big for the budget even alone: dropped, nothing is evicted.
        let before = s.stats();
        assert_eq!(s.insert(msg("a", &[5; 700])), Insert::Dropped);
        assert_eq!(s.stats().used_bytes, before.used_bytes);
        assert_eq!(s.stats().list_rows, before.list_rows);
        assert_eq!(s.stats().drops, before.drops + 1);
        audit(&s);
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
            if emptied(&s) > before {
                // A latest value was evicted: nothing older than the new message was left.
                assert_eq!((s.hist_order.len(), s.list.len()), (1, 1), "at {i}");
                latest_gone = true;
            } else if s.list.len() < had.1 + 1 {
                // A list row was evicted: the history holds only the new entry.
                assert_eq!(s.hist_order.len(), 1, "at {i}");
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
        let mut s = TopicStore::new(Limits {
            budget,
            list_len,
            ..big()
        });
        let total = TOPICS * 20;
        for i in 0..total {
            stored(s.insert(msg(&topic(i), &payload(i as u64))));
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
            st.history_bytes <= 20 * (HIST_OVERHEAD + 100),
            "history was evicted"
        );
        assert!(s.hist_order.len() <= 20 && s.hist_order.len() < total / 10);
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
            node_budget: 2 * n,
            ..big()
        });
        for i in 0..300 {
            let p: &[u8] = if i % 3 == 0 { b"" } else { b"x" };
            stored(s.insert(msg("a", p)));
            stored(s.insert(msg("b", p)));
            audit(&s);
        }
        assert_eq!(s.stats().drops, 0);

        // Exactly one empty message fits. One byte less does not.
        let mut t = TopicStore::new(Limits {
            budget: n + per_msg(0) - 1,
            node_budget: n,
            ..big()
        });
        assert_eq!(t.insert(msg("a", b"")), Insert::Dropped);
        assert_eq!(t.stats().topics, 0, "a dropped message creates no node");
        let mut t = TopicStore::new(Limits {
            budget: n + per_msg(0),
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
        assert_eq!(stored(s.insert(msg("z", b"1"))), NodeId(0));
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
        assert!(st.get(NodeId(u32::MAX)).is_none());
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

    #[test]
    fn eviction_work_per_insert_is_capped() {
        let limits = mid_limits();
        let mut st = TopicStore::new(limits);
        // Fill the budget with many small values.
        let mut i = 0;
        while st.stats().used_bytes < limits.budget - 64 * 1024 && i < 9_000 {
            st.insert(msg(&format!("t/{i}"), &[0u8; 600]));
            i += 1;
        }
        let before = st.stats();
        // A big value needs more than MAX_EVICTIONS_PER_INSERT evictions: it is dropped.
        let big = vec![1u8; 1024 * 1024];
        assert_eq!(st.insert(msg("big", &big)), Insert::Dropped);
        assert_eq!(st.stats().drops, before.drops + 1);
        assert!(st.stats().used_bytes <= limits.budget);
        // A message that needs few evictions is still stored.
        assert!(matches!(
            st.insert(msg("t/0", &[2u8; 700])),
            Insert::Stored(_)
        ));
        audit(&st);
    }

    #[test]
    fn failed_replacement_keeps_the_old_value() {
        let limits = mid_limits();
        let mut st = TopicStore::new(limits);
        // "first" is the oldest value.
        st.insert(msg("first", b"old value"));
        let mut i = 0;
        while st.stats().used_bytes < limits.budget - 64 * 1024 && i < 9_000 {
            st.insert(msg(&format!("t/{i}"), &[0u8; 600]));
            i += 1;
        }
        // Replacing "first" needs more than MAX_EVICTIONS_PER_INSERT evictions: it is dropped.
        let big = vec![1u8; 1024 * 1024];
        assert_eq!(st.insert(msg("first", &big)), Insert::Dropped);
        let id = st.find("first").expect("node");
        let old = st.node(id).latest.expect("value");
        assert_eq!(&old.payload[..], b"old value");
        assert!(!old.truncated);
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
        assert_eq!(s.history(NodeId(99)).count(), 0);
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
            budget: 3 * (ROW_OVERHEAD + SENT_EXTRA + 3 + ARC_OVERHEAD),
            ..big()
        });
        for op in 0..20 {
            assert!(matches!(sent(&mut t, op, "abc", b""), Insert::Recorded(_)));
            audit(&t);
        }
        assert_eq!(t.stats().list_rows, 3);
        assert!(t.stats().list_drops >= 17);
        assert_eq!(t.ops.len(), 3);
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
            budget: n + 4 * per_msg(200),
            node_budget: n,
            ..big()
        });
        for i in 0..3u8 {
            stored(s.insert(msg("a", &[i; 200])));
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
        assert_eq!(s.colour_of(NodeId(999)), None);
        s.set_subscriptions(vec!["#".into(), "factory/#".into(), "$SYS/#".into()]);
        assert_eq!(s.colour_of(t), Some(0));
        assert_eq!(s.colour_of(sys), Some(2));
        assert_eq!(s.colour_of_topic("x/y"), Some(0));
        assert_eq!(s.colour_of_topic("$x"), None);
        s.set_subscriptions(vec!["a/b#".into()]);
        assert_eq!(s.colour_of(t), None, "an invalid filter matches nothing");
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
            assert!(s.stats().used_bytes <= limits.budget, "over budget at {i}");
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
}
