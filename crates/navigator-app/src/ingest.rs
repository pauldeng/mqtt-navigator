//! The time plan of one merge allocation: how long the merge takes messages from the net queue,
//! and how long `TopicStore::work` runs after it. The functions here do not use gpui, so the
//! tests can run them with a real store.

use std::time::{Duration, Instant};

use navigator_core::store::TopicStore;

/// Most merge work on the UI thread in one allocation.
const FRAME_BUDGET: Duration = Duration::from_millis(4);
/// What the merge leaves unused at the end of the allocation. One `work` round that starts before
/// its deadline can end after it, and the anchor and the counters need time too.
const HEADROOM: Duration = Duration::from_micros(750);
/// The end of the maintenance, counted from the start of the allocation.
const WORK_END: Duration = FRAME_BUDGET.saturating_sub(HEADROOM);
/// Part of the allocation that the merge leaves for maintenance while the store has work.
const WORK_RESERVE: Duration = Duration::from_millis(1);

/// Deadlines of one allocation, counted from its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The merge takes no new message after this time.
    pub ingest: Duration,
    /// `TopicStore::work` stops after this time (it always runs one round).
    pub work: Duration,
}

/// The plan of the next allocation. The merge never ingests up to the end of the allocation, so
/// a round of maintenance that starts during the ingest still ends inside `FRAME_BUDGET`.
pub fn plan(has_work: bool) -> Plan {
    Plan {
        ingest: if has_work {
            FRAME_BUDGET - WORK_RESERVE
        } else {
            WORK_END
        },
        work: WORK_END,
    }
}

/// Runs one allocation. `pull` takes one message from the queue and handles it. It returns false
/// when the queue is empty. It always runs once: the caller may hold a message already.
pub fn run(
    store: &mut TopicStore,
    start: Instant,
    plan: Plan,
    mut pull: impl FnMut(&mut TopicStore) -> bool,
) {
    let ingest_end = start + plan.ingest;
    while pull(store) && Instant::now() < ingest_end {}
    if store.has_work() {
        store.work(start + plan.work);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;

    use bytes::Bytes;
    use navigator_core::Incoming;
    use navigator_core::model::{MsgProps, Qos};
    use navigator_core::net::Protocol;
    use navigator_core::store::{Limits, ListFilter, TreeFilter};

    use super::*;

    pub fn msg(topic: String, payload: Bytes) -> Incoming {
        Incoming {
            generation: 1,
            topic: Arc::from(topic),
            payload,
            qos: Qos::AtMostOnce,
            retain: false,
            props: MsgProps::default(),
            received_at: SystemTime::now(),
            protocol: Protocol::V5,
        }
    }

    #[test]
    fn the_plan_keeps_headroom_for_maintenance() {
        for work in [false, true] {
            let p = plan(work);
            assert!(p.ingest <= p.work, "{p:?}");
            assert!(p.work + HEADROOM <= FRAME_BUDGET, "{p:?}");
        }
        assert!(plan(true).ingest < plan(false).ingest);
    }

    /// A large tree (more than 100k nodes), a tree filter, a list filter and a store at its
    /// memory budget: no allocation ends much after its plan. The clock is real, so the margin
    /// is generous (and larger in a debug build).
    #[test]
    fn an_allocation_ends_near_its_plan_with_a_large_tree_and_filters() {
        let limits = Limits {
            budget: 24 * 1024 * 1024,
            node_budget: 256 * 1024 * 1024,
            ..Limits::default()
        };
        let mut s = TopicStore::new(limits);
        let payload = Bytes::from(vec![b'x'; 1000]);
        for i in 0..110_000u32 {
            s.insert(msg(
                format!("site{}/line{}/dev{i}", i % 10, (i / 10) % 100),
                payload.clone(),
            ));
        }
        s.set_list_filter(ListFilter {
            text: "xx".into(),
            ..ListFilter::default()
        });
        s.set_tree_filter(TreeFilter {
            text: "dev1".into(),
            ..TreeFilter::default()
        });
        let margin = if cfg!(debug_assertions) {
            Duration::from_millis(6)
        } else {
            Duration::from_micros(750)
        };
        let (mut i, mut worst) = (0u32, Duration::ZERO);
        let mut plans = [plan(false), plan(true)].into_iter().cycle();
        for round in 0..400 {
            let p = plans.next().expect("cycle");
            let start = Instant::now();
            run(&mut s, start, p, |st| {
                i += 1;
                st.insert(msg(
                    format!("new{}/line{}/dev{i}", i % 7, i % 13),
                    payload.clone(),
                ));
                true
            });
            let took = start.elapsed();
            worst = worst.max(took.saturating_sub(p.work));
            assert!(
                took <= p.work + margin,
                "round {round}: {took:?} for a plan of {p:?}"
            );
        }
        println!("allocation overrun past the work deadline: worst {worst:?}");
        assert!(i > 0);
    }
}
