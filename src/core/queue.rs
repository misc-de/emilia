//! Queue navigation decisions, free of GTK and the player: which queue index
//! "next" and "previous" lead to, the shuffle order, and the capped history
//! stacks. [`crate::ui::app_playback_nav`] asks these functions what to do and
//! then carries it out (loading audio, MPRIS, saving the queue), so the rules
//! themselves can be tested without a running app.

/// How far into a track "previous" still steps **back** instead of restarting
/// it. Past this the first press sends the running track to its start and only
/// a second one steps to the track before it — what music players have taught
/// everyone to expect, while the first seconds stay usable for paging back
/// through a list quickly.
pub const PREV_RESTART_MS: i64 = 3_000;

/// Most entries kept in the "recently played" history (for stepping back
/// across playback contexts).
pub const HISTORY_CAP: usize = 200;

/// Most displaced playback contexts kept on the back stack.
pub const NAV_STACK_CAP: usize = 50;

/// Pushes `item` and drops the oldest entries beyond `cap`.
pub fn push_capped<T>(stack: &mut Vec<T>, item: T, cap: usize) {
    stack.push(item);
    if stack.len() > cap {
        let excess = stack.len() - cap;
        stack.drain(..excess);
    }
}

/// Random order of the queue indices for shuffle, and how far it has got.
/// Every track of the queue plays exactly once per round.
#[derive(Debug, Default, Clone)]
pub struct ShuffleOrder {
    order: Vec<usize>,
    idx: usize,
}

impl ShuffleOrder {
    /// New random order (Fisher-Yates) over `len` tracks with `current` in first
    /// place, so the running track isn't skipped right away. `rand(n)` returns
    /// a uniform index in `0..n`.
    pub fn rebuild(&mut self, len: usize, current: usize, rand: &mut impl FnMut(usize) -> usize) {
        let mut order: Vec<usize> = (0..len).collect();
        for i in (1..len).rev() {
            order.swap(i, rand(i + 1));
        }
        if let Some(p) = order.iter().position(|&x| x == current) {
            order.swap(0, p);
        }
        self.order = order;
        self.idx = 0;
    }

    /// Forgets the order; the next step builds a fresh one.
    pub fn clear(&mut self) {
        self.order.clear();
        self.idx = 0;
    }

    /// The track the order starts with.
    pub fn first(&self) -> Option<usize> {
        self.order.first().copied()
    }

    /// The order still describes a queue of `len` tracks positioned at `current`.
    /// Not so after the queue changed or the user picked a track by hand.
    fn tracks(&self, len: usize, current: usize) -> bool {
        self.order.len() == len && self.order.get(self.idx) == Some(&current)
    }

    /// The shuffled successor of `current`, or `None` when the round is over.
    /// A stale order is rebuilt from `current` first.
    fn advance(
        &mut self,
        len: usize,
        current: usize,
        rand: &mut impl FnMut(usize) -> usize,
    ) -> Option<usize> {
        if !self.tracks(len, current) {
            self.rebuild(len, current, rand);
        }
        let next = *self.order.get(self.idx + 1)?;
        self.idx += 1;
        Some(next)
    }
}

/// The queue index "next" moves to, or `None` when playback should stop.
///
/// `shuffle` is the shuffle order when shuffling is on. With `wrap` (repeat,
/// or an explicit user "next") the end of the queue starts over from the top —
/// a new shuffle round when shuffling; a single track restarts itself.
pub fn next_index(
    len: usize,
    pos: usize,
    shuffle: Option<&mut ShuffleOrder>,
    wrap: bool,
    rand: &mut impl FnMut(usize) -> usize,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let step = match shuffle {
        Some(order) => {
            let step = order.advance(len, pos, rand);
            if step.is_none() && wrap {
                order.rebuild(len, pos, rand);
                return order.first();
            }
            step
        }
        None => (pos + 1 < len).then_some(pos + 1),
    };
    step.or(wrap.then_some(0))
}

/// Where an explicitly enqueued track goes: right after the running one
/// (at the start of an empty queue). Returns its new index.
pub fn splice_after<T>(queue: &mut Vec<T>, pos: usize, item: T) -> usize {
    let at = if queue.is_empty() {
        0
    } else {
        (pos + 1).min(queue.len())
    };
    queue.insert(at, item);
    at
}

/// What the player looks like when "previous" is pressed.
pub struct PrevInput<'a, T> {
    pub queue: &'a [T],
    pub pos: usize,
    /// A queue track is loaded in the player.
    pub playing: bool,
    /// Position shown in the bar (ms).
    pub position_ms: i64,
    /// The track that ran out when playback came to a stop.
    pub last_finished: Option<&'a T>,
    /// The back stack of displaced contexts is not empty.
    pub has_displaced_context: bool,
    /// The most recently played track from the history.
    pub last_history: Option<&'a T>,
}

/// What "previous" should do, in the order the presses are meant.
#[derive(Debug, PartialEq, Eq)]
pub enum Prev {
    /// Playback ran out → play `last_finished` again, at this queue index if
    /// it is still in the queue, otherwise as a queue of its own.
    ReplayFinished(Option<usize>),
    /// Send the running track back to its start.
    Restart,
    /// Step to this queue index.
    Step(usize),
    /// Restore the context a single-song tap displaced (pop the back stack).
    RestoreContext,
    /// Play the most recent history entry (pop it), at this queue index if it
    /// is in the queue, otherwise as a queue of its own.
    History(Option<usize>),
    /// Nothing to go back to.
    Nothing,
}

/// Decides what "previous" does:
///
/// 1. Playback ran out and stopped → play that track again.
/// 2. The running track is past [`PREV_RESTART_MS`] → send it back to its
///    start. A second press then falls through to the step below, which is
///    how "back, back" reaches the previous track.
/// 3. Step to the **previous track** of the running queue.
/// 4. At the very start of the queue (or a lone single-song context):
///    restore a context that a single-song tap displaced, then the most
///    recently played track, and finally — with nothing before it — a
///    restart of the current track.
pub fn prev_action<T: PartialEq>(s: &PrevInput<'_, T>) -> Prev {
    let index_of = |item: &T| s.queue.iter().position(|q| q == item);
    if !s.playing
        && let Some(done) = s.last_finished
    {
        return Prev::ReplayFinished(index_of(done));
    }
    if s.playing && s.position_ms > PREV_RESTART_MS {
        return Prev::Restart;
    }
    if s.pos > 0 && s.queue.len() > 1 {
        return Prev::Step(s.pos - 1);
    }
    if s.has_displaced_context {
        return Prev::RestoreContext;
    }
    if let Some(prev) = s.last_history {
        return Prev::History(index_of(prev));
    }
    if s.playing {
        Prev::Restart
    } else {
        Prev::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic "random": always the last index (no swap in Fisher-Yates).
    fn no_shuffle(n: usize) -> usize {
        n - 1
    }

    /// Deterministic "random": always the first index.
    fn first(_: usize) -> usize {
        0
    }

    fn input<'a>(queue: &'a [u32], pos: usize) -> PrevInput<'a, u32> {
        PrevInput {
            queue,
            pos,
            playing: true,
            position_ms: 0,
            last_finished: None,
            has_displaced_context: false,
            last_history: None,
        }
    }

    #[test]
    fn push_capped_drops_the_oldest() {
        let mut s = vec![1, 2, 3];
        push_capped(&mut s, 4, 3);
        assert_eq!(s, [2, 3, 4]);
        push_capped(&mut s, 5, 5);
        assert_eq!(s, [2, 3, 4, 5]);
    }

    #[test]
    fn sequential_next_steps_then_stops_or_wraps() {
        assert_eq!(next_index(3, 0, None, false, &mut first), Some(1));
        assert_eq!(next_index(3, 2, None, false, &mut first), None);
        assert_eq!(next_index(3, 2, None, true, &mut first), Some(0));
        // A single track restarts itself on an explicit "next".
        assert_eq!(next_index(1, 0, None, true, &mut first), Some(0));
        assert_eq!(next_index(0, 0, None, true, &mut first), None);
    }

    #[test]
    fn shuffle_rebuild_puts_the_current_track_first() {
        let mut o = ShuffleOrder::default();
        for current in 0..5 {
            o.rebuild(5, current, &mut first);
            assert_eq!(o.first(), Some(current));
            let mut sorted = o.order.clone();
            sorted.sort();
            assert_eq!(sorted, [0, 1, 2, 3, 4]);
        }
    }

    #[test]
    fn shuffle_plays_every_track_once_per_round() {
        let mut o = ShuffleOrder::default();
        let mut pos = 2;
        let mut seen = vec![pos];
        while let Some(n) = next_index(4, pos, Some(&mut o), false, &mut no_shuffle) {
            seen.push(n);
            pos = n;
        }
        seen.sort();
        assert_eq!(seen, [0, 1, 2, 3]);
    }

    #[test]
    fn shuffle_with_wrap_starts_a_new_round() {
        let mut o = ShuffleOrder::default();
        let mut pos = 0;
        // Three tracks: the running one plus two steps make up the round.
        for _ in 0..2 {
            pos = next_index(3, pos, Some(&mut o), false, &mut no_shuffle).unwrap();
        }
        // Round over: without wrap it stops, with wrap a new round begins.
        let mut stopped = o.clone();
        assert_eq!(
            next_index(3, pos, Some(&mut stopped), false, &mut no_shuffle),
            None
        );
        let restart = next_index(3, pos, Some(&mut o), true, &mut no_shuffle);
        assert_eq!(restart, Some(pos));
    }

    #[test]
    fn shuffle_resyncs_after_a_manual_pick() {
        let mut o = ShuffleOrder::default();
        o.rebuild(4, 0, &mut no_shuffle);
        // The user jumped to track 3 by hand: the order no longer tracks it,
        // so it is rebuilt from 3 and steps on from there.
        let next = next_index(4, 3, Some(&mut o), false, &mut no_shuffle).unwrap();
        assert_ne!(next, 3);
        assert_eq!(o.first(), Some(3));
    }

    #[test]
    fn splice_after_inserts_behind_the_running_track() {
        let mut q = vec![10, 11, 12];
        assert_eq!(splice_after(&mut q, 0, 99), 1);
        assert_eq!(q, [10, 99, 11, 12]);
        assert_eq!(splice_after(&mut q, 3, 98), 4);
        assert_eq!(q, [10, 99, 11, 12, 98]);
        let mut empty = Vec::new();
        assert_eq!(splice_after(&mut empty, 5, 1), 0);
    }

    #[test]
    fn prev_replays_a_finished_track_first() {
        let q = [1, 2, 3];
        let mut s = input(&q, 2);
        s.playing = false;
        s.last_finished = Some(&3);
        s.has_displaced_context = true;
        assert_eq!(prev_action(&s), Prev::ReplayFinished(Some(2)));
        s.last_finished = Some(&7);
        assert_eq!(prev_action(&s), Prev::ReplayFinished(None));
    }

    #[test]
    fn prev_restarts_past_the_threshold_then_steps_back() {
        let q = [1, 2, 3];
        let mut s = input(&q, 1);
        s.position_ms = PREV_RESTART_MS + 1;
        assert_eq!(prev_action(&s), Prev::Restart);
        s.position_ms = PREV_RESTART_MS;
        assert_eq!(prev_action(&s), Prev::Step(0));
    }

    #[test]
    fn prev_at_the_queue_start_falls_back_in_order() {
        let q = [1, 2];
        let mut s = input(&q, 0);
        s.has_displaced_context = true;
        s.last_history = Some(&2);
        assert_eq!(prev_action(&s), Prev::RestoreContext);
        s.has_displaced_context = false;
        assert_eq!(prev_action(&s), Prev::History(Some(1)));
        s.last_history = Some(&9);
        assert_eq!(prev_action(&s), Prev::History(None));
        s.last_history = None;
        assert_eq!(prev_action(&s), Prev::Restart);
        s.playing = false;
        assert_eq!(prev_action(&s), Prev::Nothing);
    }

    #[test]
    fn prev_in_a_single_track_queue_does_not_step() {
        let q = [1];
        let s = input(&q, 0);
        assert_eq!(prev_action(&s), Prev::Restart);
    }
}
