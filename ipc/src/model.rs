//! `ModelTransport` (plan `doc/plans/2_ipc.md` §3.4): a deterministic in-memory
//! kernel implementing `Transport`, so Shuttle/Loom can schedule the
//! communicating processes (sender, receiver) over the shared channel +
//! notification objects — the cross-process races where the real concurrency
//! lives (§2 of the plan).
//!
//! Faithful to the kernel it models:
//!   - the channel is a bounded FIFO ring (`send` → `Full`, `recv` → `Empty`);
//!   - the notification is `kcore::notification` exactly — `signal` ORs bits
//!     into the word and wakes a waiter or accumulates; `wait` consumes the
//!     word if non-zero, **else blocks** (the `while word == 0` check below is
//!     the lost-wakeup guard the harnesses exercise);
//!   - a send fires the persistent on-readable binding (§3.6).
//!
//! Compiled only for the model/harnesses; built on `crate::sync` so the same
//! code runs under std (smoke), loom (exhaustive), and shuttle (randomized).

use crate::sync::{Arc, Condvar, Mutex};
use crate::transport::{Chan, Event, Notif, RecvErr, RecvOk, SendErr, Timer, Transport};
use std::collections::VecDeque;
use std::vec::Vec;

struct ModelMsg {
    data: Vec<u8>,
    /// Cap slots, ABI-faithful: `SLOT_NONE` (`u32::MAX`) marks an absent cap.
    caps: [u32; 4],
}

/// One channel: a bounded FIFO ring plus its event bindings (§3.3, §3.6).
struct Ring {
    msgs: VecDeque<ModelMsg>,
    peer_closed: bool,
    on_readable: Option<(Notif, u64)>,
    on_writable: Option<(Notif, u64)>,
    on_peer_closed: Option<(Notif, u64)>,
}

/// One notification object: a word + a condvar (§3.6). The `Mutex<u64>` word is
/// the source of truth; the condvar is only the wake mechanism, so a notify
/// that races ahead of a waiter is harmless — the waiter re-checks the word.
struct Notification {
    word: Mutex<u64>,
    cv: Condvar,
}

/// A deterministic in-memory kernel: one channel (`Chan` is ignored — a single
/// channel for now) and a fixed set of notifications. Shared across model
/// threads via `crate::sync::Arc`.
pub struct ModelTransport {
    ring: Mutex<Ring>,
    cap: usize,
    notifs: Vec<Notification>,
}

impl ModelTransport {
    /// A channel of capacity `cap` slots (§3.2) and `num_notifs` notifications.
    pub fn new(cap: usize, num_notifs: usize) -> ModelTransport {
        let mut notifs = Vec::with_capacity(num_notifs);
        for _ in 0..num_notifs {
            notifs.push(Notification { word: Mutex::new(0), cv: Condvar::new() });
        }
        ModelTransport {
            ring: Mutex::new(Ring {
                msgs: VecDeque::new(),
                peer_closed: false,
                on_readable: None,
                on_writable: None,
                on_peer_closed: None,
            }),
            cap,
            notifs,
        }
    }

    /// Convenience wrapper: a fresh `ModelTransport` behind a model `Arc`.
    pub fn shared(cap: usize, num_notifs: usize) -> Arc<ModelTransport> {
        Arc::new(ModelTransport::new(cap, num_notifs))
    }
}

impl Transport for ModelTransport {
    fn send_nb(&self, _ch: Chan, data: &[u8], caps: Option<&[u32; 4]>) -> Result<(), SendErr> {
        let binding = {
            let mut ring = self.ring.lock().unwrap();
            if ring.peer_closed {
                return Err(SendErr::Closed);
            }
            if ring.msgs.len() >= self.cap {
                return Err(SendErr::Full);
            }
            ring.msgs.push_back(ModelMsg {
                data: data.to_vec(),
                caps: caps.copied().unwrap_or([crate::sys::SLOT_NONE; 4]),
            });
            ring.on_readable
        };
        // Fire the on-readable binding *after* releasing the ring lock (the
        // kernel signals from the send path; holding both locks is needless).
        if let Some((n, bits)) = binding {
            self.notif_signal(n, bits);
        }
        Ok(())
    }

    fn recv_nb(&self, _ch: Chan, buf: &mut [u8], _dests: Option<&[u32; 4]>) -> Result<RecvOk, RecvErr> {
        let mut ring = self.ring.lock().unwrap();
        match ring.msgs.pop_front() {
            Some(msg) => {
                let len = msg.data.len().min(buf.len());
                buf[..len].copy_from_slice(&msg.data[..len]);
                let mut cap_mask = 0u64;
                for (i, c) in msg.caps.iter().enumerate() {
                    if *c != crate::sys::SLOT_NONE {
                        cap_mask |= 1 << i;
                    }
                }
                Ok(RecvOk { len, cap_mask })
            }
            None if ring.peer_closed => Err(RecvErr::Closed),
            None => Err(RecvErr::Empty),
        }
    }

    fn bind(&self, _ch: Chan, ev: Event, notif: Notif, bits: u64) -> Result<(), i64> {
        let mut ring = self.ring.lock().unwrap();
        let slot = match ev {
            Event::Readable => &mut ring.on_readable,
            Event::Writable => &mut ring.on_writable,
            Event::PeerClosed => &mut ring.on_peer_closed,
        };
        *slot = Some((notif, bits));
        Ok(())
    }

    fn notif_signal(&self, n: Notif, bits: u64) {
        let notif = &self.notifs[n as usize];
        let mut word = notif.word.lock().unwrap();
        *word |= bits;
        // Wake one waiter if present; if none, the word stays set (accumulates)
        // and the next `wait` consumes it without blocking.
        notif.cv.notify_one();
    }

    fn notif_wait(&self, n: Notif) -> u64 {
        let notif = &self.notifs[n as usize];
        let mut word = notif.word.lock().unwrap();
        // Check the accumulated word before sleeping — the lost-wakeup guard.
        while *word == 0 {
            word = notif.cv.wait(word).unwrap();
        }
        let w = *word;
        *word = 0;
        w
    }

    fn timer_arm(&self, _t: Timer, _n: Notif, _bits: u64, _delta: u64) {
        // Timers enter with the reactor (plan §4.3, phase 3); the Phase-0 rig
        // smoke needs none, and a logical-clock model lands with them.
        unimplemented!("ModelTransport timer modeling: reactor phase");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{thread, Arc};

    // The Phase-0 rig smoke: a sender process enqueues one message (firing the
    // on-readable binding) while a receiver process binds, polls, and — if the
    // poll is Empty — waits, then receives. The message must always arrive (no
    // lost wakeup at the rig level). This is NOT the reactor (phase 2) — a
    // hand-inlined poll-then-wait that proves the rig is drivable by both tools,
    // the scratchpad analogue for `ipc`.
    fn rig_smoke() {
        let t = ModelTransport::shared(2, 1);
        // Bind on-readable -> (notif 0, bit 1) before the race (the §3.6 "bind
        // first" half of the discipline).
        t.bind(0, Event::Readable, 0, 1).unwrap();

        let ts = Arc::clone(&t);
        let sender = thread::spawn(move || {
            ts.send_nb(0, &[42u8], None).unwrap();
        });

        let tr = Arc::clone(&t);
        let receiver = thread::spawn(move || -> u8 {
            let mut buf = [0u8; 256];
            loop {
                match tr.recv_nb(0, &mut buf, None) {
                    Ok(_) => return buf[0],
                    Err(RecvErr::Empty) => {
                        tr.notif_wait(0);
                    }
                    Err(e) => panic!("unexpected recv error: {:?}", e),
                }
            }
        });

        sender.join().unwrap();
        let got = receiver.join().unwrap();
        assert_eq!(got, 42, "the receiver must observe the sent message");
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn rig_smoke_std() {
        rig_smoke();
    }

    #[cfg(loom)]
    #[test]
    fn rig_smoke_loom() {
        loom::model(rig_smoke);
    }

    #[cfg(shuttle)]
    #[test]
    fn rig_smoke_shuttle() {
        shuttle::check_random(rig_smoke, 1000);
    }

    // Harness #3 (plan doc/plans/2_ipc.md §5.2): FIFO / no double-delivery under
    // concurrent senders, over the *typed* Endpoint (the real Phase-1 code).
    // Two sender processes each send `per_sender` distinct ids on one channel
    // (A: 1..=k, B: 101..=100+k); a receiver drains all 2k. Capacity = 2k, so
    // Full never fires here — backpressure/retry is harness #2 (phase 3) — and
    // no notifications are used (waiting is harness #1, phase 2): pure poll.
    // Gated to the tiers that drive it (std + shuttle); there is no loom variant.
    #[cfg(not(loom))]
    fn fifo_no_drop(per_sender: u8) {
        use crate::endpoint::{Endpoint, Message};
        use crate::transport::Chan;
        const CHAN: Chan = 0;
        let total = 2 * per_sender as usize;
        let t = ModelTransport::shared(total, 0);

        let ta = Arc::clone(&t);
        let sa = thread::spawn(move || {
            let ep = Endpoint::new(&*ta, CHAN);
            for i in 1..=per_sender {
                ep.send_nb(&Message::bytes(&[i])).unwrap();
            }
        });

        let tb = Arc::clone(&t);
        let sb = thread::spawn(move || {
            let ep = Endpoint::new(&*tb, CHAN);
            for i in 1..=per_sender {
                ep.send_nb(&Message::bytes(&[100 + i])).unwrap();
            }
        });

        let tr = Arc::clone(&t);
        let receiver = thread::spawn(move || -> std::vec::Vec<u8> {
            let ep = Endpoint::new(&*tr, CHAN);
            let mut got = std::vec::Vec::new();
            let mut msg = Message::new();
            while got.len() < total {
                match ep.recv_nb(&mut msg) {
                    Ok(()) => got.push(msg.payload()[0]),
                    Err(RecvErr::Empty) => thread::yield_now(),
                    Err(e) => panic!("unexpected recv error: {:?}", e),
                }
            }
            got
        });

        sa.join().unwrap();
        sb.join().unwrap();
        let got = receiver.join().unwrap();

        // No drop / no double-delivery: exactly the sent set, once each.
        let mut sorted = got.clone();
        sorted.sort_unstable();
        let mut expected: std::vec::Vec<u8> =
            (1..=per_sender).chain(101..=100 + per_sender).collect();
        expected.sort_unstable();
        assert_eq!(sorted, expected, "every id received exactly once (no drop, no dup)");

        // Per-sender FIFO: each sender's ids arrive in increasing (send) order,
        // since the channel is FIFO (§3.3).
        let a: std::vec::Vec<u8> = got.iter().copied().filter(|&x| x <= per_sender).collect();
        assert!(a.windows(2).all(|w| w[0] < w[1]), "sender A not FIFO: {:?}", a);
        let b: std::vec::Vec<u8> = got.iter().copied().filter(|&x| x > 100).collect();
        assert!(b.windows(2).all(|w| w[0] < w[1]), "sender B not FIFO: {:?}", b);
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn fifo_no_drop_std() {
        fifo_no_drop(2);
    }

    // Shuttle is harness #3's tier (interleaving/SC); loom is reserved for the
    // phase-2 lost-wakeup fragment (§5.3), so there is no loom variant here.
    #[cfg(shuttle)]
    #[test]
    fn fifo_no_drop_shuttle() {
        shuttle::check_random(|| fifo_no_drop(2), 1000);
    }

    // Harness #1 (plan §5.2/§5.3): no lost wakeup, over the *real* Reactor (the
    // phase-2 §4.2 code). A sender sends one message; a receiver registers the
    // channel for readable on a Reactor, then loops wait() -> recv_nb. The
    // threads race — the send may land before *or* after the receiver's bind,
    // and inside the wait window — and the message must always be received.
    // poll-once (register's self-signal) catches the send-before-bind case; the
    // notif_wait word-check catches the in-window race. Deleting register's
    // self-signal makes the send-before-bind interleaving deadlock (the negative
    // control), which Loom/Shuttle report.
    fn reactor_no_lost_wakeup() {
        use crate::endpoint::{Endpoint, Message};
        use crate::reactor::{Reactor, Signals};
        use crate::transport::Chan;
        const CHAN: Chan = 0;
        const NOTIF: crate::transport::Notif = 0;
        const KEY: crate::reactor::Key = 7;
        let t = ModelTransport::shared(2, 1);

        let ts = Arc::clone(&t);
        let sender = thread::spawn(move || {
            let ep = Endpoint::new(&*ts, CHAN);
            ep.send_nb(&Message::bytes(&[42u8])).unwrap();
        });

        let tr = Arc::clone(&t);
        let receiver = thread::spawn(move || -> u8 {
            let mut reactor = Reactor::new(&*tr, NOTIF);
            reactor.register(CHAN, Signals::READABLE, KEY).unwrap();
            let ep = Endpoint::new(&*tr, CHAN);
            let mut msg = Message::new();
            loop {
                let (key, _signals) = reactor.wait();
                assert_eq!(key, KEY, "wait returned the wrong source key");
                match ep.recv_nb(&mut msg) {
                    Ok(()) => return msg.payload()[0],
                    Err(RecvErr::Empty) => {} // spurious wakeup — re-wait
                    Err(e) => panic!("unexpected recv error: {:?}", e),
                }
            }
        });

        sender.join().unwrap();
        let got = receiver.join().unwrap();
        assert_eq!(got, 42, "the receiver must observe the sent message (no lost wakeup)");
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn reactor_no_lost_wakeup_std() {
        reactor_no_lost_wakeup();
    }

    #[cfg(shuttle)]
    #[test]
    fn reactor_no_lost_wakeup_shuttle() {
        shuttle::check_random(reactor_no_lost_wakeup, 1000);
    }

    // The §5.3 weak-memory fragment: the poll-then-wait sequence against the
    // notification word, exhaustively, at a tiny bound (one message).
    #[cfg(loom)]
    #[test]
    fn reactor_no_lost_wakeup_loom() {
        loom::model(reactor_no_lost_wakeup);
    }
}
