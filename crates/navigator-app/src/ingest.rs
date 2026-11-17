//! The time plan of one merge allocation: how long the merge takes messages from the net queue,
//! and how long `TopicStore::work` runs after it. The functions here do not use gpui, so the
//! tests can run them with a real store.

use std::time::{Duration, Instant};

use navigator_core::Incoming;
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
/// Backlog mode starts when the net queue holds more than this many messages ...
const ENTER_MSGS: u64 = 1024;
/// ... or more than this many bytes.
const ENTER_BYTES: usize = 4 * 1024 * 1024;
/// Backlog mode ends when the queue holds fewer messages than this, and fewer bytes than
/// `LEAVE_BYTES`. The gap to `ENTER_*` stops a flip-flop.
const LEAVE_MSGS: u64 = 256;
const LEAVE_BYTES: usize = 1024 * 1024;
/// Ingest time in backlog mode, and when the store is above its budget (eviction is due).
const BACKLOG_INGEST: Duration = Duration::from_micros(2500);
const BACKLOG_INGEST_OVER_BUDGET: Duration = Duration::from_micros(1500);

/// Backlog mode: the net queue is deep, so the merge runs one allocation in every display frame
/// and does not wait for the coalesce time.
#[derive(Debug, Default)]
pub struct Backlog {
    on: bool,
}

impl Backlog {
    /// Updates the mode from the queue (messages and bytes) and returns it.
    pub fn update(&mut self, msgs: u64, bytes: usize) -> bool {
        if self.on {
            if msgs < LEAVE_MSGS && bytes < LEAVE_BYTES {
                self.on = false;
            }
        } else if msgs > ENTER_MSGS || bytes > ENTER_BYTES {
            self.on = true;
        }
        self.on
    }

    pub fn on(&self) -> bool {
        self.on
    }
}

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
///
/// Backlog mode ingests 2.5 ms. Above the store budget it ingests 1.5 ms, so that eviction gets
/// the rest. Outside backlog mode the merge ingests 3.25 ms, or 3 ms while the store has work.
pub fn plan(has_work: bool, backlog: bool, over_budget: bool) -> Plan {
    let ingest = match (backlog, over_budget) {
        (true, true) => BACKLOG_INGEST_OVER_BUDGET,
        (true, false) => BACKLOG_INGEST,
        (false, _) if has_work => FRAME_BUDGET - WORK_RESERVE,
        (false, _) => WORK_END,
    };
    Plan {
        ingest,
        work: WORK_END,
    }
}

/// The message that starts the next allocation. `None`: the net queue or the kick channel is
/// closed and the loop must end. `Some(None)`: a kick, no message.
///
/// While `pending` (the store has work) it does not wait: it takes a message if there is one.
/// Otherwise it parks on the queue and on the kick channel. There is no timer, so an idle loop
/// never wakes.
pub async fn next_first(
    rx: &mut tokio::sync::mpsc::Receiver<Incoming>,
    kick: &mut tokio::sync::mpsc::Receiver<()>,
    pending: bool,
) -> Option<Option<Incoming>> {
    if pending {
        return Some(rx.try_recv().ok());
    }
    tokio::select! {
        m = rx.recv() => m.map(Some),
        k = kick.recv() => k.map(|()| None),
    }
}

/// Where the time of one allocation went.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timing {
    /// Messages that `pull` handled, and the time they took.
    pub messages: u32,
    pub ingest: Duration,
    /// Time of `TopicStore::work`.
    pub work: Duration,
}

/// Runs one allocation. `pull` takes one message from the queue and handles it. It returns false
/// when the queue is empty. It always runs once: the caller may hold a message already.
pub fn run(
    store: &mut TopicStore,
    start: Instant,
    plan: Plan,
    mut pull: impl FnMut(&mut TopicStore) -> bool,
) -> Timing {
    let ingest_end = start + plan.ingest;
    let mut t = Timing::default();
    while pull(store) {
        t.messages += 1;
        if Instant::now() >= ingest_end {
            break;
        }
    }
    let work_start = Instant::now();
    t.ingest = work_start - start;
    if store.has_work() {
        store.work(start + plan.work);
        t.work = work_start.elapsed();
    }
    t
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;

    use bytes::Bytes;
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
        for (work, backlog, over) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let p = plan(work, backlog, over);
            assert!(p.ingest <= p.work, "{p:?}");
            assert!(p.work + HEADROOM <= FRAME_BUDGET, "{p:?}");
        }
        assert!(plan(true, false, false).ingest < plan(false, false, false).ingest);
    }

    #[test]
    fn the_backlog_budget_is_split_as_planned() {
        let ms = Duration::from_micros;
        assert_eq!(plan(false, true, false).ingest, ms(2500));
        assert_eq!(plan(true, true, false).ingest, ms(2500));
        assert_eq!(plan(true, true, true).ingest, ms(1500));
        assert_eq!(plan(true, true, true).work, ms(3250));
        // Outside the backlog the old split stays: no wait until the store is over budget.
        assert_eq!(plan(false, false, true), plan(false, false, false));
        assert_eq!(HEADROOM, ms(750));
    }

    #[test]
    fn backlog_mode_has_hysteresis() {
        let mut b = Backlog::default();
        // Not above the thresholds: off.
        assert!(!b.update(1024, 4 * 1024 * 1024));
        // One message or one byte above: on.
        assert!(b.update(1025, 0));
        // Between the thresholds it stays on.
        assert!(b.update(300, 2 * 1024 * 1024));
        assert!(
            b.update(255, 1024 * 1024),
            "bytes at the limit are not below it"
        );
        assert!(b.update(256, 0), "messages at the limit are not below it");
        // Both below: off.
        assert!(!b.update(255, 1024 * 1024 - 1));
        // Between the thresholds it stays off.
        assert!(!b.update(1000, 3 * 1024 * 1024));
        // The byte threshold alone enters.
        assert!(b.update(10, 4 * 1024 * 1024 + 1));
        // The bytes alone keep the mode on.
        assert!(b.update(0, 1024 * 1024));
        assert!(!b.update(0, 1024 * 1024 - 1));
    }

    /// Counts wake-ups of a task.
    struct Wakes(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// An idle loop parks on the queue: it has no timer, and only a message or a kick wakes it.
    #[test]
    fn an_idle_loop_parks_until_an_event() {
        use std::future::Future;
        use std::sync::atomic::Ordering::SeqCst;
        use std::task::{Context, Poll};

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (kick_tx, mut kick) = tokio::sync::mpsc::channel(1);
        let wakes = Arc::new(Wakes(Default::default()));
        let waker = std::task::Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(next_first(&mut rx, &mut kick, false));
        for _ in 0..3 {
            assert!(fut.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(wakes.0.load(SeqCst), 0, "nothing woke the parked loop");
        kick_tx.try_send(()).expect("room");
        assert_eq!(wakes.0.load(SeqCst), 1, "a kick wakes it once");
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Some(None))
        ));
        drop(fut);
        let mut fut = Box::pin(next_first(&mut rx, &mut kick, false));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        tx.try_send(msg("a/b".into(), Bytes::new())).expect("room");
        assert_eq!(wakes.0.load(SeqCst), 2, "a message wakes it once");
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Some(Some(_)))
        ));
        drop(fut);
        // With pending work it does not park: no message gives `Some(None)` at once.
        let mut fut = Box::pin(next_first(&mut rx, &mut kick, true));
        assert!(matches!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(Some(None))
        ));
        drop(fut);
        // A closed queue ends the loop.
        drop(tx);
        let mut fut = Box::pin(next_first(&mut rx, &mut kick, false));
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(None)));
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
        let mut i = 0u32;
        let mut over = Vec::new();
        let mut plans = [
            plan(false, false, false),
            plan(true, false, false),
            plan(true, true, false),
            plan(true, true, true),
        ]
        .into_iter()
        .cycle();
        for _ in 0..400 {
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
            over.push(start.elapsed().saturating_sub(p.work));
        }
        over.sort();
        let (p95, worst) = (over[over.len() * 95 / 100], over[over.len() - 1]);
        println!("allocation overrun past the work deadline: p95 {p95:?}, worst {worst:?}");
        // The OS can stop the test thread (the tests run in parallel), so only the 95th
        // percentile has the tight bound. The worst case gets a loose one.
        assert!(p95 <= margin, "p95 {p95:?}");
        assert!(
            worst <= margin + Duration::from_millis(5),
            "worst {worst:?}"
        );
        assert!(i > 0);
    }
}
