//! Control slots: how Rust asks the Python pump for gateway actions without holding Python objects
//! (W2, §6.4, §12.3).
//!
//! `CtrlQueue` is a set of slots, not a queue: every control is level-triggered or coalesces to its
//! latest value, so nothing is ever dropped however long the loop stalls.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::CloseReason;
use crate::wake::{WAKE_CTRL, WakeHub};

/// A request for the Python pump (§6.4). There is no `TrackEnded`: completion is level-triggered
/// through `WAKE_TRACKS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    ChannelMoved { channel_id: u64 },
    ClassifyUsers,
    Rejoin { attempt: u32, channel_id: u64 },
    Closed { reason: CloseReason },
}

/// The four slots (under L10).
#[derive(Debug, Default)]
struct Slots {
    moved: Option<u64>,
    classify: bool,
    rejoin: Option<(u32, u64)>,
    closed: Option<CloseReason>,
}

/// Coalescing control slots; setting one rings `WAKE_CTRL`.
#[derive(Debug)]
pub struct CtrlQueue {
    /// L10.
    slots: Mutex<Slots>,
    wake: Arc<WakeHub>,
}

impl CtrlQueue {
    pub fn new(wake: Arc<WakeHub>) -> Arc<Self> {
        Arc::new(CtrlQueue {
            slots: Mutex::new(Slots::default()),
            wake,
        })
    }

    /// Latest value wins; `Closed` is sticky (the first reason is kept, and a pending `Rejoin` is
    /// dropped once closed); rings `WAKE_CTRL` after unlocking.
    pub fn set(&self, c: Control) {
        {
            let mut s = self.slots.lock();
            match c {
                Control::ChannelMoved { channel_id } => s.moved = Some(channel_id),
                Control::ClassifyUsers => s.classify = true,
                Control::Rejoin {
                    attempt,
                    channel_id,
                } => {
                    if s.closed.is_some() {
                        return;
                    }
                    s.rejoin = Some((attempt, channel_id));
                }
                Control::Closed { reason } => {
                    if s.closed.is_none() {
                        s.closed = Some(reason);
                    }
                    s.rejoin = None;
                }
            }
        }
        self.wake.notify(WAKE_CTRL);
    }

    /// Sets the sticky `Closed` slot (T8).
    pub fn set_closed(&self, r: CloseReason) {
        self.set(Control::Closed { reason: r });
    }

    /// Takes every pending slot in the table order of §6.4 and clears them; `Rejoin` is omitted
    /// once `Closed` is set. `Closed` is level-triggered: it is reported by every `take` once set.
    pub fn take(&self, out: &mut Vec<Control>) {
        self.take_up_to(usize::MAX, out);
    }

    /// Like [`CtrlQueue::take`], but takes at most `max` slots; the others stay pending.
    pub fn take_up_to(&self, max: usize, out: &mut Vec<Control>) {
        let mut s = self.slots.lock();
        let mut n = 0usize;
        if n < max
            && let Some(channel_id) = s.moved.take()
        {
            out.push(Control::ChannelMoved { channel_id });
            n += 1;
        }
        if n < max && std::mem::take(&mut s.classify) {
            out.push(Control::ClassifyUsers);
            n += 1;
        }
        if s.closed.is_some() {
            s.rejoin = None;
        }
        if n < max
            && let Some((attempt, channel_id)) = s.rejoin.take()
        {
            out.push(Control::Rejoin {
                attempt,
                channel_id,
            });
            n += 1;
        }
        if n < max
            && let Some(reason) = s.closed
        {
            out.push(Control::Closed { reason });
        }
    }

    /// The sticky close reason, once set.
    pub fn closed(&self) -> Option<CloseReason> {
        self.slots.lock().closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake::Doorbell;
    use std::os::unix::net::UnixStream;

    fn queue() -> (Arc<CtrlQueue>, Arc<WakeHub>, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        let wake = WakeHub::new(Doorbell::from_unix_stream(a).unwrap());
        (CtrlQueue::new(wake.clone()), wake, b)
    }

    #[test]
    fn slots_coalesce_and_come_out_in_table_order() {
        let (q, wake, _peer) = queue();
        q.set(Control::Rejoin {
            attempt: 1,
            channel_id: 5,
        });
        q.set(Control::ClassifyUsers);
        q.set(Control::ChannelMoved { channel_id: 1 });
        q.set(Control::ChannelMoved { channel_id: 2 });
        q.set(Control::Rejoin {
            attempt: 2,
            channel_id: 6,
        });
        q.set(Control::ClassifyUsers);
        let mut ready = Vec::new();
        wake.take_ready(&mut ready);
        assert_eq!(ready, vec![WAKE_CTRL.0]);
        let mut out = Vec::new();
        q.take(&mut out);
        assert_eq!(
            out,
            vec![
                Control::ChannelMoved { channel_id: 2 },
                Control::ClassifyUsers,
                Control::Rejoin {
                    attempt: 2,
                    channel_id: 6
                },
            ]
        );
        out.clear();
        q.take(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn closed_is_sticky_and_suppresses_rejoin() {
        let (q, _wake, _peer) = queue();
        q.set(Control::Rejoin {
            attempt: 1,
            channel_id: 5,
        });
        q.set_closed(CloseReason::Kicked);
        q.set_closed(CloseReason::Requested);
        q.set(Control::Rejoin {
            attempt: 2,
            channel_id: 5,
        });
        let mut out = Vec::new();
        q.take(&mut out);
        assert_eq!(
            out,
            vec![Control::Closed {
                reason: CloseReason::Kicked
            }]
        );
        out.clear();
        q.take(&mut out);
        assert_eq!(
            out,
            vec![Control::Closed {
                reason: CloseReason::Kicked
            }]
        );
        assert_eq!(q.closed(), Some(CloseReason::Kicked));
    }

    #[test]
    fn take_up_to_keeps_the_rest() {
        let (q, _wake, _peer) = queue();
        q.set(Control::ChannelMoved { channel_id: 3 });
        q.set(Control::ClassifyUsers);
        let mut out = Vec::new();
        q.take_up_to(1, &mut out);
        assert_eq!(out, vec![Control::ChannelMoved { channel_id: 3 }]);
        out.clear();
        q.take_up_to(0, &mut out);
        assert!(out.is_empty());
        q.take(&mut out);
        assert_eq!(out, vec![Control::ClassifyUsers]);
    }
}
