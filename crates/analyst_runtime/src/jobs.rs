use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Result of submitting work to a newest-wins lane.
#[derive(Debug, Eq, PartialEq)]
pub enum SubmitOutcome<T> {
    Enqueued,
    Replaced(T),
}

/// Error returned when the worker side has already closed.
#[derive(Eq, PartialEq)]
pub struct SendClosed<T>(pub T);

impl<T> fmt::Debug for SendClosed<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SendClosed(..)")
    }
}

struct Pending<L, T> {
    lane: L,
    value: T,
}

struct ChannelState<L, T> {
    pending: VecDeque<Pending<L, T>>,
    sender_count: usize,
    receiver_open: bool,
}

struct Shared<L, T> {
    state: Mutex<ChannelState<L, T>>,
    available: Condvar,
}

/// Producer for a bounded-by-lane newest-wins channel.
///
/// At most one request is queued for each distinct lane. Submitting another
/// request for that lane removes and returns the old request, then places the
/// new request at the back of the queue. Moving replacements to the back keeps
/// a rapidly changing pane from starving requests already waiting for other
/// panes.
pub struct LatestLaneSender<L, T> {
    shared: Arc<Shared<L, T>>,
}

/// Single consumer for [`LatestLaneSender`].
pub struct LatestLaneReceiver<L, T> {
    shared: Arc<Shared<L, T>>,
}

pub fn latest_lane_channel<L, T>() -> (LatestLaneSender<L, T>, LatestLaneReceiver<L, T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(ChannelState {
            pending: VecDeque::new(),
            sender_count: 1,
            receiver_open: true,
        }),
        available: Condvar::new(),
    });
    (
        LatestLaneSender {
            shared: Arc::clone(&shared),
        },
        LatestLaneReceiver { shared },
    )
}

impl<L, T> Clone for LatestLaneSender<L, T> {
    fn clone(&self) -> Self {
        let mut state = lock_state(&self.shared);
        state.sender_count = state.sender_count.saturating_add(1);
        drop(state);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<L: Eq, T> LatestLaneSender<L, T> {
    pub fn submit(&self, lane: L, value: T) -> Result<SubmitOutcome<T>, SendClosed<T>> {
        let mut state = lock_state(&self.shared);
        if !state.receiver_open {
            return Err(SendClosed(value));
        }

        let replaced = state
            .pending
            .iter()
            .position(|pending| pending.lane == lane)
            .and_then(|index| state.pending.remove(index))
            .map(|pending| pending.value);
        state.pending.push_back(Pending { lane, value });
        drop(state);
        self.shared.available.notify_one();

        Ok(match replaced {
            Some(value) => SubmitOutcome::Replaced(value),
            None => SubmitOutcome::Enqueued,
        })
    }

    pub fn clear_lane(&self, lane: &L) -> Option<T> {
        let mut state = lock_state(&self.shared);
        state
            .pending
            .iter()
            .position(|pending| &pending.lane == lane)
            .and_then(|index| state.pending.remove(index))
            .map(|pending| pending.value)
    }

    pub fn queued_lanes(&self) -> usize {
        lock_state(&self.shared).pending.len()
    }
}

impl<L, T> Drop for LatestLaneSender<L, T> {
    fn drop(&mut self) {
        let mut state = lock_state(&self.shared);
        state.sender_count = state.sender_count.saturating_sub(1);
        let last_sender = state.sender_count == 0;
        drop(state);
        if last_sender {
            self.shared.available.notify_all();
        }
    }
}

impl<L, T> LatestLaneReceiver<L, T> {
    /// Wait for the next request. Returns `None` after all senders have been
    /// dropped and every already-queued request has been drained.
    pub fn recv(&self) -> Option<(L, T)> {
        let mut state = lock_state(&self.shared);
        loop {
            if let Some(pending) = state.pending.pop_front() {
                return Some((pending.lane, pending.value));
            }
            if state.sender_count == 0 || !state.receiver_open {
                return None;
            }
            state = wait_for_work(&self.shared, state);
        }
    }

    pub fn try_recv(&self) -> Option<(L, T)> {
        lock_state(&self.shared)
            .pending
            .pop_front()
            .map(|pending| (pending.lane, pending.value))
    }

    pub fn queued_lanes(&self) -> usize {
        lock_state(&self.shared).pending.len()
    }

    /// Close the worker side and discard queued requests. Future submissions
    /// fail and return their payload to the caller.
    pub fn close(&self) {
        let mut state = lock_state(&self.shared);
        state.receiver_open = false;
        state.pending.clear();
        drop(state);
        self.shared.available.notify_all();
    }
}

impl<L, T> Drop for LatestLaneReceiver<L, T> {
    fn drop(&mut self) {
        let mut state = lock_state(&self.shared);
        state.receiver_open = false;
        state.pending.clear();
        drop(state);
        self.shared.available.notify_all();
    }
}

fn lock_state<L, T>(shared: &Shared<L, T>) -> MutexGuard<'_, ChannelState<L, T>> {
    shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_for_work<'a, L, T>(
    shared: &Shared<L, T>,
    state: MutexGuard<'a, ChannelState<L, T>>,
) -> MutexGuard<'a, ChannelState<L, T>> {
    shared
        .available
        .wait(state)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_request_replaces_only_its_lane_and_moves_to_back() {
        let (sender, receiver) = latest_lane_channel();
        assert_eq!(sender.submit(0_u8, "a1"), Ok(SubmitOutcome::Enqueued));
        assert_eq!(sender.submit(1_u8, "b1"), Ok(SubmitOutcome::Enqueued));
        assert_eq!(sender.submit(0_u8, "a2"), Ok(SubmitOutcome::Replaced("a1")));

        assert_eq!(receiver.try_recv(), Some((1, "b1")));
        assert_eq!(receiver.try_recv(), Some((0, "a2")));
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn dropping_all_senders_closes_after_pending_work_is_drained() {
        let (sender, receiver) = latest_lane_channel();
        sender.submit(3_u8, 9_u8).unwrap();
        drop(sender);
        assert_eq!(receiver.recv(), Some((3, 9)));
        assert_eq!(receiver.recv(), None);
    }

    #[test]
    fn closing_receiver_returns_unsent_payload() {
        let (sender, receiver) = latest_lane_channel();
        receiver.close();
        let error = sender.submit(0_u8, String::from("request")).unwrap_err();
        assert_eq!(error.0, "request");
    }
}
