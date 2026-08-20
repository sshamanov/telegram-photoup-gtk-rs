use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A fixed pool of `nproc − 2` workers. Each worker runs complete photo jobs
/// (decode→process→encode) so single-threaded libs (libraw, mozjpeg) scale by
/// running independent jobs across workers — never by threading inside the libs.
/// Jobs are dispatched round-robin; each worker drains its own queue, so in-flight
/// work is bounded to roughly the worker count. Completion is signaled on a shared
/// `results` channel (one `()` per finished job).
pub struct ImagePool {
    senders: Vec<Sender<Job>>,
    next: std::sync::atomic::AtomicUsize,
    results: Receiver<()>,
}

impl ImagePool {
    pub fn new() -> Self {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(2);
        Self::with_workers(workers)
    }

    pub fn with_workers(count: usize) -> Self {
        let count = count.max(1); // `% count` below must never divide by zero
        let (result_tx, result_rx) = channel::<()>();
        let mut senders = Vec::with_capacity(count);
        for i in 0..count {
            let (tx, rx) = channel::<Job>();
            senders.push(tx);
            let rtx = result_tx.clone();
            let _ = thread::Builder::new()
                .name(format!("img-worker-{i}"))
                .spawn(move || {
                    for job in rx.iter() {
                        // A panicking job must still signal completion, or wait_one
                        // hangs forever and the worker silently dies.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job()));
                        let _ = rtx.send(());
                    }
                });
        }
        Self {
            senders,
            next: std::sync::atomic::AtomicUsize::new(0),
            results: result_rx,
        }
    }

    /// Run `f` on the next worker; the pool signals completion on `results` when done.
    pub fn submit<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.senders.len();
        let _ = self.senders[idx].send(Box::new(f));
    }

    /// Block until one job finishes (used by the GTK main loop via a timeout poll).
    pub fn wait_one(&self) {
        let _ = self.results.recv();
    }

    pub fn try_wait_one(&self) -> bool {
        self.results.try_recv().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn runs_jobs_and_signals_completion() {
        let pool = ImagePool::with_workers(4);
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..8 {
            let d = Arc::clone(&done);
            pool.submit(move || {
                std::thread::sleep(Duration::from_millis(20));
                d.fetch_add(1, Ordering::SeqCst);
            });
        }
        // Wait for all 8 completions.
        let start = Instant::now();
        for _ in 0..8 {
            pool.wait_one();
        }
        assert_eq!(done.load(Ordering::SeqCst), 8);
        // With 4 workers and 8×20ms jobs, wall time should be ~40-80ms, not 160ms.
        let elapsed = start.elapsed().as_millis();
        assert!(elapsed < 150, "elapsed {elapsed}ms — jobs did not run concurrently");
    }
}
