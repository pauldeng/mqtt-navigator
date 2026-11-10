//! Topic store: the live topic tree and the latest message of each topic.
//!
//! P1 scope: latest value only (no history), payload truncation, a topic cap, a node-metadata
//! budget and a total budget. The UI owns one `TopicStore` and calls it on the UI thread, so
//! every method must be fast. See the TopicStore section of the plan and ADR 0003.

use std::collections::BTreeMap;
use std::time::SystemTime;

use bytes::Bytes;

use crate::model::{Incoming, MsgProps, Qos};

/// Most old values one insert may evict. If more space is needed, the message is dropped.
pub const MAX_EVICTIONS_PER_INSERT: usize = 1024;

/// Most topic levels. A deeper topic is dropped (counted as a topic drop).
pub const MAX_TOPIC_LEVELS: usize = 64;

/// Index of a node in the arena. Stable until `clear`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// Store limits. All values are in bytes except `topic_cap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest stored latest-value payload. A longer payload is truncated (copied).
    pub latest_payload_max: usize,
    /// Most topic nodes (internal and leaf).
    pub topic_cap: usize,
    /// Budget for all stored data (latest values, properties, node metadata).
    pub budget: usize,
    /// Budget for node metadata only. Must not be more than `budget`.
    pub node_budget: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            latest_payload_max: 1024 * 1024,
            topic_cap: 200_000,
            budget: 256 * 1024 * 1024,
            node_budget: 64 * 1024 * 1024,
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

/// What `insert` did with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    Stored(NodeId),
    /// The topic is new and the topic cap or the node budget is full.
    TopicDropped,
    /// The message does not fit in the budget.
    Dropped,
}

/// Read-only view of one node, for rendering one row.
#[derive(Clone, Copy, Debug)]
pub struct NodeView<'a> {
    pub id: NodeId,
    /// Last topic level, for example `temp` in `factory/line1/temp`.
    pub name: &'a str,
    pub depth: u16,
    pub has_children: bool,
    pub expanded: bool,
    /// Messages received on this exact topic.
    pub msg_count: u64,
    /// Messages received on this topic and all topics below it.
    pub subtree_msgs: u64,
    /// Topics with a message below this node (not counting this node).
    pub subtree_topics: u32,
    pub latest: Option<&'a Msg>,
}

/// Counters for the status bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    pub topics: usize,
    pub messages: u64,
    pub used_bytes: usize,
    pub topic_drops: u64,
    pub drops: u64,
}

/// One node of the arena. Internal nodes and leaves look the same.
struct Node {
    name: Box<str>,
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
}

/// Estimated bytes of one node besides its name: the struct and one entry in the parent map.
/// The name is stored twice (node and parent map key), so the name counts twice.
const NODE_OVERHEAD: usize = size_of::<Node>() + 48;

fn node_cost(name: &str) -> usize {
    NODE_OVERHEAD + 2 * name.len()
}

/// Bytes that a stored message costs. It does not include the node.
fn msg_cost(msg: &Msg) -> usize {
    msg.payload.len() + msg.props.heap_bytes()
}

pub struct TopicStore {
    limits: Limits,
    /// Arena. A `NodeId` is an index.
    nodes: Vec<Node>,
    /// Children of the virtual root. Topics start at depth 0.
    roots: BTreeMap<Box<str>, NodeId>,
    /// Visible nodes in display order. Valid only when `rows_dirty` is false.
    rows: Vec<NodeId>,
    rows_dirty: bool,
    /// Nodes with a non-empty latest value, oldest first. Key is the receive sequence.
    order: BTreeMap<u64, NodeId>,
    seq: u64,
    /// Bytes of node metadata.
    node_bytes: usize,
    /// Bytes of all latest values (payload and properties).
    payload_bytes: usize,
    messages: u64,
    topic_drops: u64,
    drops: u64,
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
        };
        Self {
            limits,
            nodes: Vec::new(),
            roots: BTreeMap::new(),
            rows: Vec::new(),
            rows_dirty: false,
            order: BTreeMap::new(),
            seq: 0,
            node_bytes: 0,
            payload_bytes: 0,
            messages: 0,
            topic_drops: 0,
            drops: 0,
        }
    }

    /// Removes all nodes and messages. Counters reset too.
    pub fn clear(&mut self) {
        *self = Self::new(self.limits);
    }

    /// Inserts one message. Creates missing nodes. Updates counts on the path.
    /// Never exceeds the budgets. `msg.generation` is not checked here.
    ///
    /// The store checks the whole cost first and changes nothing when it returns a drop.
    /// If the message does not fit, the store evicts the oldest latest values of other topics
    /// (payload and properties become empty, `truncated` is set, the node stays).
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
        let (new_nodes, new_node_bytes) = missing
            .clone()
            .fold((0, 0), |(n, b), part| (n + 1, b + node_cost(part)));

        // Admission: compute the full cost before any change.
        let original_size = payload.len();
        let stored_len = original_size.min(self.limits.latest_payload_max);
        let cost = stored_len + props.heap_bytes();
        if new_nodes > 0
            && (self.nodes.len() + new_nodes > self.limits.topic_cap
                || self.node_bytes + new_node_bytes > self.limits.node_budget)
        {
            self.topic_drops += 1;
            return Insert::TopicDropped;
        }
        if self.node_bytes + new_node_bytes + cost > self.limits.budget {
            // Does not fit even if every other latest value is evicted.
            self.drops += 1;
            return Insert::Dropped;
        }

        // Evict the oldest values until the message fits. The old value of this topic counts as
        // free space, but it is released only when the message is stored. The eviction work for
        // one insert is capped, so one message cannot stall the UI thread.
        let existing = if new_nodes == 0 { parent } else { None };
        let own_cost = |st: &Self| {
            existing.map_or(0, |id| {
                st.nodes[id.0 as usize].latest.as_ref().map_or(0, msg_cost)
            })
        };
        let mut evicted = 0;
        while self.node_bytes + self.payload_bytes - own_cost(self) + new_node_bytes + cost
            > self.limits.budget
        {
            if evicted == MAX_EVICTIONS_PER_INSERT || !self.evict_oldest(existing) {
                self.drops += 1;
                return Insert::Dropped;
            }
            evicted += 1;
        }
        if let Some(id) = existing {
            self.release(id);
        }
        debug_assert!(
            self.node_bytes + self.payload_bytes + new_node_bytes + cost <= self.limits.budget
        );

        // Create the missing nodes.
        let parent_visible = parent.is_none_or(|p| self.nodes[p.0 as usize].expanded);
        let mut leaf = parent;
        for (i, name) in missing.enumerate() {
            let id = NodeId(self.nodes.len() as u32); // topic_cap <= u32::MAX is checked above
            let depth = leaf.map_or(0, |p| self.nodes[p.0 as usize].depth.saturating_add(1));
            self.nodes.push(Node {
                name: name.into(),
                parent: leaf,
                depth,
                children: BTreeMap::new(),
                expanded: false,
                msg_count: 0,
                subtree_msgs: 0,
                subtree_topics: 0,
                latest: None,
                seq: 0,
            });
            self.node_bytes += node_cost(name);
            match leaf {
                None => self.roots.insert(name.into(), id),
                Some(p) => self.nodes[p.0 as usize].children.insert(name.into(), id),
            };
            // Only the first new node can show up in the rows: the others are below a new
            // (collapsed) node.
            if i == 0 && parent_visible {
                self.rows_dirty = true;
            }
            leaf = Some(id);
        }
        let Some(id) = leaf else {
            // A topic has at least one level, so this cannot happen.
            return Insert::Dropped;
        };

        // Store the value. A truncated payload is copied so it does not keep the big buffer.
        let truncated = stored_len < original_size;
        let payload = if truncated {
            Bytes::copy_from_slice(&payload[..stored_len])
        } else {
            payload
        };
        let msg = Msg {
            payload,
            truncated,
            original_size,
            qos,
            retain,
            props,
            received_at,
        };
        let node = &mut self.nodes[id.0 as usize];
        let first = node.msg_count == 0;
        node.msg_count += 1;
        node.latest = Some(msg);
        if cost > 0 {
            self.seq += 1;
            node.seq = self.seq;
            self.order.insert(self.seq, id);
            self.payload_bytes += cost;
        }
        self.messages += 1;

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
        Insert::Stored(id)
    }

    /// Visible rows in display order (depth first, children a–z, only below expanded nodes).
    /// Rebuilds the list only if the structure or the expansion changed.
    pub fn rows(&mut self) -> &[NodeId] {
        if self.rows_dirty {
            // ponytail: full rebuild, O(visible rows). Each new node under an expanded node
            // costs one rebuild. Make it an incremental splice if P1 measures a problem.
            let mut rows = std::mem::take(&mut self.rows);
            rows.clear();
            let mut stack = vec![self.roots.values()];
            while let Some(it) = stack.last_mut() {
                match it.next() {
                    Some(&id) => {
                        rows.push(id);
                        let n = &self.nodes[id.0 as usize];
                        if n.expanded {
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
    pub fn get(&self, id: NodeId) -> Option<NodeView<'_>> {
        let n = self.nodes.get(id.0 as usize)?;
        Some(NodeView {
            id,
            name: &n.name,
            depth: n.depth,
            has_children: !n.children.is_empty(),
            expanded: n.expanded,
            msg_count: n.msg_count,
            subtree_msgs: n.subtree_msgs,
            subtree_topics: n.subtree_topics,
            latest: n.latest.as_ref(),
        })
    }

    /// Full topic path of a node, for example `factory/line1/temp`.
    /// An unknown id gives an empty string.
    pub fn path(&self, id: NodeId) -> String {
        let mut names = Vec::new();
        let mut cur = Some(id);
        while let Some(n) = cur.and_then(|c| self.nodes.get(c.0 as usize)) {
            names.push(&*n.name);
            cur = n.parent;
        }
        names.reverse();
        names.join("/")
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
    pub fn set_expanded(&mut self, id: NodeId, expanded: bool) {
        if let Some(n) = self.nodes.get_mut(id.0 as usize)
            && n.expanded != expanded
        {
            n.expanded = expanded;
            self.rows_dirty = true;
        }
    }

    pub fn stats(&self) -> StoreStats {
        StoreStats {
            topics: self.nodes.len(),
            messages: self.messages,
            used_bytes: self.node_bytes + self.payload_bytes,
            topic_drops: self.topic_drops,
            drops: self.drops,
        }
    }

    /// `None` is the virtual root.
    fn children_of(&self, id: Option<NodeId>) -> &BTreeMap<Box<str>, NodeId> {
        match id {
            None => &self.roots,
            Some(id) => &self.nodes[id.0 as usize].children,
        }
    }

    /// Takes the bytes of the latest value of a node out of the accounting.
    /// The caller replaces the value right after.
    fn release(&mut self, id: NodeId) {
        let n = &self.nodes[id.0 as usize];
        let cost = n.latest.as_ref().map_or(0, msg_cost);
        if cost > 0 {
            self.order.remove(&n.seq);
            self.payload_bytes -= cost;
        }
    }

    /// Empties the oldest non-empty latest value, except the value of `keep` (the topic that is
    /// being replaced: it is released only when its new value is stored). Returns false if there
    /// is nothing to evict.
    fn evict_oldest(&mut self, keep: Option<NodeId>) -> bool {
        let Some((&seq, &id)) = self.order.iter().find(|(_, id)| Some(**id) != keep) else {
            return false;
        };
        self.order.remove(&seq);
        if let Some(m) = self.nodes[id.0 as usize].latest.as_mut() {
            self.payload_bytes -= msg_cost(m);
            m.payload = Bytes::new();
            m.props = MsgProps::default(); // ponytail: props go too, so the cost is exactly 0
            m.truncated = true;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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

    fn stored(r: Insert) -> NodeId {
        match r {
            Insert::Stored(id) => id,
            other => panic!("expected Stored, got {other:?}"),
        }
    }

    fn row_paths(s: &mut TopicStore) -> Vec<String> {
        let rows = s.rows().to_vec();
        rows.into_iter().map(|id| s.path(id)).collect()
    }

    /// Recomputes every counter from the nodes and compares it with the store.
    fn audit(s: &TopicStore) {
        let nodes: usize = s.nodes.iter().map(|n| node_cost(&n.name)).sum();
        let msgs: usize = s
            .nodes
            .iter()
            .filter_map(|n| n.latest.as_ref())
            .map(msg_cost)
            .sum();
        assert_eq!(s.node_bytes, nodes);
        assert_eq!(s.payload_bytes, msgs);
        let live = s
            .nodes
            .iter()
            .filter(|n| n.latest.as_ref().is_some_and(|m| msg_cost(m) > 0));
        assert_eq!(s.order.len(), live.count());
        for (seq, id) in &s.order {
            assert_eq!(s.nodes[id.0 as usize].seq, *seq);
        }
        assert!(s.node_bytes + s.payload_bytes <= s.limits.budget);
        assert!(s.node_bytes <= s.limits.node_budget);
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
        assert_eq!(s.stats().used_bytes, node_cost("t") + 4);

        // Exactly at the limit: kept as is, same buffer.
        let exact = Bytes::from_static(b"abcd");
        let id = stored(s.insert(incoming("t", exact.clone(), MsgProps::default())));
        let m = s.node(id).latest.expect("latest");
        assert!(!m.truncated);
        assert_eq!(m.payload.as_ptr(), exact.as_ptr());
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
        audit(&s);
    }

    #[test]
    fn node_budget_drops_new_topics() {
        let limits = Limits {
            node_budget: 2 * node_cost("x"),
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
        };
        let mut s = TopicStore::new(limits);
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let (mut stored_n, mut topic_dropped, mut dropped) = (0, 0, 0);
        for i in 0..4000 {
            let t = next() % 500;
            let topic = format!(
                "{}/{}/{}",
                "site-with-a-rather-long-name".repeat(1 + (t % 3) as usize),
                t % 7,
                format!("device-{t}-").repeat(1 + (t % 5) as usize)
            );
            let len = match next() % 4 {
                0 => 0,
                1 => 1,
                2 => (next() % 500) as usize,
                _ => (next() % 40_000) as usize,
            };
            let props = MsgProps {
                content_type: (i % 5 == 0).then(|| {
                    // Now and then a huge property, so that some messages never fit.
                    let max = if i % 50 == 0 { 120_000 } else { 2000 };
                    "x".repeat((next() % max) as usize)
                }),
                ..MsgProps::default()
            };
            match s.insert(incoming(&topic, Bytes::from(vec![7u8; len]), props)) {
                Insert::Stored(_) => stored_n += 1,
                Insert::TopicDropped => topic_dropped += 1,
                Insert::Dropped => dropped += 1,
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
    }

    #[test]
    fn eviction_keeps_metadata() {
        let n = node_cost("a");
        let limits = Limits {
            budget: 2 * n + 1000,
            node_budget: 2 * n,
            ..big()
        };
        let mut s = TopicStore::new(limits);
        let a = stored(s.insert(msg("a", &[1; 600])));
        let b = stored(s.insert(msg("b", &[2; 600]))); // evicts a
        let ma = s.node(a).latest.expect("a keeps its message record");
        assert!(ma.payload.is_empty() && ma.truncated);
        assert_eq!(ma.original_size, 600);
        assert_eq!(s.node(a).msg_count, 1);
        assert_eq!(s.find("a"), Some(a));
        assert_eq!(s.node(b).latest.expect("b").payload.len(), 600);
        assert_eq!(s.stats().used_bytes, 2 * n + 600);
        audit(&s);

        // Replace b by a bigger value: the old cost is freed first, nothing else is evicted.
        stored(s.insert(msg("b", &[3; 900])));
        assert_eq!(s.node(b).latest.expect("b").payload.len(), 900);
        // Now a needs the room: b is evicted.
        stored(s.insert(msg("a", &[4; 900])));
        assert!(s.node(b).latest.expect("b").payload.is_empty());
        assert_eq!(s.node(a).latest.expect("a").payload.len(), 900);
        audit(&s);

        // Too big for the budget even alone: dropped, nothing changes.
        let before = s.stats();
        assert_eq!(s.insert(msg("a", &[5; 1001])), Insert::Dropped);
        assert_eq!(s.node(a).latest.expect("a").payload.len(), 900);
        assert_eq!(s.stats().drops, 1);
        assert_eq!(s.stats().used_bytes, before.used_bytes);
        assert_eq!(s.node(a).msg_count, 2);
        audit(&s);
    }

    #[test]
    fn empty_and_one_byte_payloads() {
        let n = node_cost("a");
        let mut s = TopicStore::new(Limits {
            budget: 2 * n + 2,
            node_budget: 2 * n,
            ..big()
        });
        let a = stored(s.insert(msg("a", b"")));
        assert_eq!(s.stats().used_bytes, n);
        assert!(!s.node(a).latest.expect("a").truncated);
        stored(s.insert(msg("b", b"x")));
        stored(s.insert(msg("a", b"y")));
        assert_eq!(s.stats().used_bytes, 2 * n + 2);
        // Replace with empty: frees the byte. Then 1-byte values keep fitting.
        stored(s.insert(msg("a", b"")));
        assert_eq!(s.stats().used_bytes, 2 * n + 1);
        for _ in 0..10 {
            stored(s.insert(msg("a", b"1")));
            stored(s.insert(msg("b", b"")));
            stored(s.insert(msg("b", b"2")));
            audit(&s);
        }
        // Zero budget left for payload: 3 bytes in total never fit.
        let mut t = TopicStore::new(Limits {
            budget: n + 2,
            node_budget: n,
            ..big()
        });
        assert_eq!(t.insert(msg("a", b"abc")), Insert::Dropped);
        assert_eq!(t.stats().topics, 0, "a dropped message creates no node");
        stored(t.insert(msg("a", b"ab")));
        audit(&t);
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
        s.clear();
        assert_eq!(s.stats(), StoreStats::default());
        assert!(s.rows().is_empty());
        assert_eq!(s.find("a"), None);
        assert_eq!(stored(s.insert(msg("z", b"1"))), NodeId(0));
        assert_eq!(
            s.insert(msg("q", b"1")),
            Insert::TopicDropped,
            "limits survive clear"
        );
        audit(&s);
    }

    #[test]
    fn bad_limits_are_fixed() {
        let mut s = TopicStore::new(Limits {
            latest_payload_max: 0,
            topic_cap: 0,
            budget: 0,
            node_budget: 0,
        });
        stored(s.insert(msg("a", b"1")));
        let small = node_cost("a") - 1;
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
    }

    #[test]
    fn eviction_work_per_insert_is_capped() {
        let limits = Limits {
            latest_payload_max: 1024 * 1024,
            topic_cap: 10_000,
            budget: 4 * 1024 * 1024,
            node_budget: 2 * 1024 * 1024,
        };
        let mut st = TopicStore::new(limits);
        // Fill the payload space with many tiny values.
        let mut i = 0;
        while st.stats().used_bytes < limits.budget - 64 * 1024 && i < 9_000 {
            st.insert(msg(&format!("t/{i}"), &[0u8; 600]));
            i += 1;
        }
        let before = st.stats();
        // A big value would need more than MAX_EVICTIONS_PER_INSERT evictions: it is dropped.
        let big = vec![1u8; 1024 * 1024];
        assert_eq!(st.insert(msg("big", &big)), Insert::Dropped);
        assert_eq!(st.stats().drops, before.drops + 1);
        assert!(st.stats().used_bytes <= limits.budget);
        // A message that needs few evictions is still stored.
        assert!(matches!(
            st.insert(msg("t/0", &[2u8; 700])),
            Insert::Stored(_)
        ));
    }

    #[test]
    fn failed_replacement_keeps_the_old_value() {
        let limits = Limits {
            latest_payload_max: 1024 * 1024,
            topic_cap: 10_000,
            budget: 4 * 1024 * 1024,
            node_budget: 2 * 1024 * 1024,
        };
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
    }
}
