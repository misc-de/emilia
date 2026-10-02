//! A tiny scoped worker pool for I/O-bound batches (feed and channel
//! refreshes): a fixed number of threads pull the next item off a shared
//! index until the list is used up. No dependency, no long-lived threads.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Runs `f` for every item on up to `threads` worker threads and returns once
/// all are done. Items are handed out in list order; `f` gets each item's
/// index. Completion order is unspecified, so progress counting belongs in `f`.
pub fn for_each<T: Sync>(items: &[T], threads: usize, f: impl Fn(usize, &T) + Sync) {
    let next = AtomicUsize::new(0);
    let workers = threads.clamp(1, items.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else {
                    break;
                };
                f(i, item);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::for_each;
    use std::sync::Mutex;

    #[test]
    fn visits_every_item_exactly_once() {
        let items: Vec<usize> = (0..100).collect();
        let seen = Mutex::new(Vec::new());
        for_each(&items, 7, |i, v| {
            assert_eq!(i, *v);
            seen.lock().unwrap().push(*v);
        });
        let mut seen = seen.into_inner().unwrap();
        seen.sort_unstable();
        assert_eq!(seen, items);
    }

    #[test]
    fn empty_list_and_zero_threads_are_fine() {
        for_each(&[] as &[u8], 4, |_, _| panic!("no items"));
        let hits = Mutex::new(0);
        for_each(&[1, 2, 3], 0, |_, _| *hits.lock().unwrap() += 1);
        assert_eq!(hits.into_inner().unwrap(), 3);
    }
}
