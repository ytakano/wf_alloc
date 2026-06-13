#![cfg(feature = "loom")]

use loom::model::Builder;
use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;
use std::time::Instant;

const NONE: usize = 0;
const UNLINKED: usize = usize::MAX;

fn rss_kb() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for key in ["VmHWM:", "VmRSS:"] {
        if let Some(line) = status.lines().find(|line| line.starts_with(key)) {
            let mut parts = line.split_whitespace();
            let _ = parts.next();
            return parts.next()?.parse().ok();
        }
    }
    None
}

fn run_loom_case<F>(name: &str, max_branches: usize, f: F)
where
    F: Fn() + Send + Sync + 'static,
{
    let start = Instant::now();
    let start_rss = rss_kb();
    let mut builder = Builder::new();
    builder.preemption_bound = Some(2);
    builder.max_branches = max_branches;
    builder.check(f);
    let elapsed = start.elapsed();
    let end_rss = rss_kb();
    match (start_rss, end_rss) {
        (Some(before), Some(after)) => eprintln!(
            "loom case {name}: elapsed={elapsed:?} rss_before={before}kB rss_after={after}kB"
        ),
        _ => eprintln!("loom case {name}: elapsed={elapsed:?} rss=unavailable"),
    }
}

const PTR_BITS: usize = 4;
const PTR_MASK: usize = (1 << PTR_BITS) - 1;
const POP_FAILED: usize = usize::MAX - 1;
const POP_EMPTY: usize = usize::MAX - 2;

fn pack_head(ptr: usize, version: usize) -> usize {
    (version << PTR_BITS) | ptr
}

fn head_ptr(word: usize) -> usize {
    word & PTR_MASK
}

fn head_version(word: usize) -> usize {
    word >> PTR_BITS
}

struct SpmcModel {
    head: AtomicUsize,
    next: [AtomicUsize; 3],
    span: [AtomicUsize; 3],
}

impl SpmcModel {
    fn with_two_spans() -> Self {
        Self {
            head: AtomicUsize::new(pack_head(0, 0)),
            next: [
                AtomicUsize::new(1),
                AtomicUsize::new(2),
                AtomicUsize::new(NONE),
            ],
            span: [
                AtomicUsize::new(NONE),
                AtomicUsize::new(2),
                AtomicUsize::new(4),
            ],
        }
    }

    fn pop_once(&self) -> usize {
        let old = self.head.load(Ordering::Acquire);
        let head = head_ptr(old);
        let next = self.next[head].load(Ordering::Acquire);
        if next == NONE {
            return POP_EMPTY;
        }
        let new = pack_head(next, head_version(old).wrapping_add(1));
        match self
            .head
            .compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => self.span[next].load(Ordering::Relaxed),
            Err(_) => POP_FAILED,
        }
    }
}

#[test]
fn loom_spmc_one_shot_pop_has_unique_successes() {
    run_loom_case("spmc_one_shot_pop", 10_000, || {
        let list = Arc::new(SpmcModel::with_two_spans());
        let a = Arc::clone(&list);
        let b = Arc::clone(&list);

        let t1 = thread::spawn(move || a.pop_once());
        let t2 = thread::spawn(move || b.pop_once());

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        let successes: Vec<_> = [r1, r2]
            .into_iter()
            .filter(|&r| r != POP_FAILED && r != POP_EMPTY)
            .collect();

        if successes.len() == 2 {
            assert_ne!(successes[0], successes[1], "same span popped twice");
        }
        assert!(successes.iter().all(|&span| span == 2 || span == 4));

        let final_head = list.head.load(Ordering::Acquire);
        assert_eq!(head_version(final_head), successes.len());
    });
}

fn pending(phase: usize) -> usize {
    (phase << 1) | 1
}

fn is_pending(value: usize) -> bool {
    value & 1 == 1
}

fn try_complete_help_record(record: &AtomicUsize, mut held_span: usize) -> usize {
    let start = record.load(Ordering::Acquire);
    if !is_pending(start) {
        return held_span;
    }
    thread::yield_now();
    let now = record.load(Ordering::Acquire);
    if now != start || !is_pending(now) {
        return held_span;
    }
    if record
        .compare_exchange(start, held_span, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        held_span = NONE;
    }
    held_span
}

#[test]
fn loom_help_record_completion_keeps_losing_span_owned() {
    run_loom_case("help_record_completion", 10_000, || {
        let record = Arc::new(AtomicUsize::new(pending(7)));
        let r1 = Arc::clone(&record);
        let r2 = Arc::clone(&record);

        let h1 = thread::spawn(move || try_complete_help_record(&r1, 2));
        let h2 = thread::spawn(move || try_complete_help_record(&r2, 4));

        let held1 = h1.join().unwrap();
        let held2 = h2.join().unwrap();
        let completed = record.swap(NONE, Ordering::AcqRel);

        assert!(completed == 2 || completed == 4);
        assert_eq!(record.load(Ordering::Acquire), NONE);
        if completed == 2 {
            assert_eq!(held1, NONE);
            assert_eq!(held2, 4);
        } else {
            assert_eq!(held1, 2);
            assert_eq!(held2, NONE);
        }
    });
}

struct RemoteMpscModel {
    head: AtomicUsize,
    next: [AtomicUsize; 2],
}

impl RemoteMpscModel {
    fn new() -> Self {
        Self {
            head: AtomicUsize::new(NONE),
            next: [AtomicUsize::new(NONE), AtomicUsize::new(NONE)],
        }
    }

    fn push_publish(&self, block: usize) -> usize {
        self.next[block].store(UNLINKED, Ordering::Relaxed);
        self.head.swap(block, Ordering::AcqRel)
    }

    fn push_link(&self, block: usize, old_head: usize) {
        self.next[block].store(old_head, Ordering::Release);
    }

    fn reclaim_all(&self) -> usize {
        self.head.swap(NONE, Ordering::AcqRel)
    }

    fn append_bounded(&self, head: usize, limit: usize) -> (usize, usize) {
        let mut appended_mask = 0usize;
        let mut cur = head;
        for _ in 0..limit {
            if cur == NONE {
                break;
            }
            let next = self.next[cur].load(Ordering::Acquire);
            if next == UNLINKED {
                return (appended_mask, cur);
            }
            appended_mask |= 1 << cur;
            cur = next;
        }
        (appended_mask, cur)
    }
}

fn chain_contains(model: &RemoteMpscModel, mut head: usize, needle: usize) -> bool {
    for _ in 0..2 {
        if head == NONE || head == UNLINKED {
            return false;
        }
        if head == needle {
            return true;
        }
        head = model.next[head].load(Ordering::Acquire);
    }
    false
}

#[test]
fn loom_remote_mpsc_unlinked_suffix_is_not_lost() {
    run_loom_case("remote_mpsc_unlinked", 10_000, || {
        let list = Arc::new(RemoteMpscModel::new());
        let producer_list = Arc::clone(&list);
        let consumer_list = Arc::clone(&list);

        let producer = thread::spawn(move || {
            let old = producer_list.push_publish(1);
            thread::yield_now();
            producer_list.push_link(1, old);
        });

        let consumer = thread::spawn(move || {
            let head = consumer_list.reclaim_all();
            consumer_list.append_bounded(head, 1)
        });

        producer.join().unwrap();
        let (appended_mask, leftover) = consumer.join().unwrap();
        let final_head = list.head.load(Ordering::Acquire);

        let in_appended = (appended_mask & (1 << 1)) != 0;
        let in_leftover = leftover == 1;
        let in_public_head = chain_contains(&list, final_head, 1);
        let owners = in_appended as usize + in_leftover as usize + in_public_head as usize;
        assert_eq!(owners, 1, "remote block must have exactly one owner");
    });
}
