//! Priority worker pool with generation-based cancellation.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Pokes the host event loop so a frame renders after off-thread work
/// posts a result. The UI host wraps its wakeup handle in one of these;
/// tests use a no-op or a channel.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

/// Queue tiers, drained in order. Within a tier, FIFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The user is looking at a spinner for this (selected-file
    /// preview, the directory listing itself).
    Urgent,
    /// Rows currently realized on screen (stat, thumbnails).
    Visible,
    /// Background fill of everything else.
    Sweep,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Queues {
    urgent: VecDeque<(u64, Job)>,
    visible: VecDeque<(u64, Job)>,
    sweep: VecDeque<(u64, Job)>,
    shutdown: bool,
}

impl Queues {
    fn pop(&mut self) -> Option<(u64, Job)> {
        self.urgent
            .pop_front()
            .or_else(|| self.visible.pop_front())
            .or_else(|| self.sweep.pop_front())
    }

    fn clear(&mut self) {
        self.urgent.clear();
        self.visible.clear();
        self.sweep.clear();
    }
}

struct Shared {
    queues: Mutex<Queues>,
    cond: Condvar,
    /// Bumped on navigation. Jobs are tagged with the generation at
    /// submit time; workers drop any job whose tag is stale by the
    /// time it reaches the front.
    generation: AtomicU64,
}

/// Handle to the pool; clones share the same workers and queues.
#[derive(Clone)]
pub struct Pool {
    shared: Arc<Shared>,
}

impl Pool {
    /// Spawn `workers` threads named `{name}-{n}`.
    pub fn spawn(workers: usize, name: &str) -> Pool {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues {
                urgent: VecDeque::new(),
                visible: VecDeque::new(),
                sweep: VecDeque::new(),
                shutdown: false,
            }),
            cond: Condvar::new(),
            generation: AtomicU64::new(0),
        });
        for n in 0..workers.max(1) {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("{name}-{n}"))
                .spawn(move || worker(shared))
                .expect("spawning io worker");
        }
        Pool { shared }
    }

    /// The current generation. Capture it at submit time and tag
    /// results with it so the UI can drop strays from a directory it
    /// already left (in-flight jobs can't be recalled, only queued
    /// ones).
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    /// Invalidate everything queued (navigation): bump the generation
    /// and drop all pending jobs. Returns the new generation.
    pub fn bump_generation(&self) -> u64 {
        let gen = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.shared.queues.lock().unwrap().clear();
        gen
    }

    /// Queue `job` at `tier`, tagged with the current generation.
    pub fn submit(&self, tier: Tier, job: impl FnOnce() + Send + 'static) {
        let gen = self.generation();
        let mut q = self.shared.queues.lock().unwrap();
        if q.shutdown {
            return;
        }
        let slot = match tier {
            Tier::Urgent => &mut q.urgent,
            Tier::Visible => &mut q.visible,
            Tier::Sweep => &mut q.sweep,
        };
        slot.push_back((gen, Box::new(job)));
        self.shared.cond.notify_one();
    }

    /// Drop queued jobs and stop the workers. In-flight jobs finish;
    /// their results land in whatever channel they captured, which the
    /// caller is free to have dropped.
    pub fn shutdown(&self) {
        let mut q = self.shared.queues.lock().unwrap();
        q.shutdown = true;
        q.clear();
        self.shared.cond.notify_all();
    }
}

fn worker(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut q = shared.queues.lock().unwrap();
            loop {
                if q.shutdown {
                    return;
                }
                if let Some((gen, job)) = q.pop() {
                    if gen != shared.generation.load(Ordering::Acquire) {
                        drop(job); // stale: navigated away since submit
                        continue;
                    }
                    break job;
                }
                q = shared.cond.wait(q).unwrap();
            }
        };
        // Arbitrary closures run here (decoders over hostile files);
        // one panic must not silently shrink the pool.
        if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(job)) {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic".into());
            tracing::error!(panic = %msg, "io job panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    /// With one worker held at a gate, later-queued Urgent work runs
    /// before earlier-queued Visible and Sweep work.
    #[test]
    fn tiers_drain_in_priority_order() {
        let pool = Pool::spawn(1, "test");
        let (tx, rx) = channel::<&'static str>();

        // Occupy the single worker so the queue builds up behind it.
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        {
            let gate = Arc::clone(&gate);
            pool.submit(Tier::Urgent, move || {
                let (lock, cond) = &*gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cond.wait(open).unwrap();
                }
            });
        }
        for (tier, tag) in [
            (Tier::Sweep, "sweep"),
            (Tier::Visible, "visible"),
            (Tier::Urgent, "urgent"),
        ] {
            let tx = tx.clone();
            pool.submit(tier, move || tx.send(tag).unwrap());
        }
        let (lock, cond) = &*gate;
        *lock.lock().unwrap() = true;
        cond.notify_all();

        let order: Vec<_> = (0..3)
            .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        assert_eq!(order, ["urgent", "visible", "sweep"]);
        pool.shutdown();
    }

    /// Jobs queued before a generation bump never run.
    #[test]
    fn bump_generation_drops_queued_jobs() {
        let pool = Pool::spawn(1, "test");
        let (tx, rx) = channel::<&'static str>();

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        {
            let gate = Arc::clone(&gate);
            pool.submit(Tier::Urgent, move || {
                let (lock, cond) = &*gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cond.wait(open).unwrap();
                }
            });
        }
        {
            let tx = tx.clone();
            pool.submit(Tier::Visible, move || tx.send("stale").unwrap());
        }
        let gen = pool.bump_generation();
        assert_eq!(pool.generation(), gen);
        {
            let tx = tx.clone();
            pool.submit(Tier::Visible, move || tx.send("fresh").unwrap());
        }
        let (lock, cond) = &*gate;
        *lock.lock().unwrap() = true;
        cond.notify_all();

        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "fresh");
        assert!(rx.try_recv().is_err(), "stale job ran");
        pool.shutdown();
    }
}
