//! `ModelTransport`: a deterministic in-memory kernel implementing `Transport`,
//! so Shuttle/Loom can schedule the communicating processes (sender, receiver)
//! over the shared channel + notification objects — the cross-process races
//! where the real concurrency lives.
//!
//! Faithful to the kernel it models:
//!   - the channel is a bounded FIFO ring (`send` → `Full`, `recv` → `Empty`);
//!   - the notification is `kcore::notification` exactly — `signal` ORs bits
//!     into the word and wakes a waiter or accumulates; `wait` consumes the
//!     word if non-zero, **else blocks** (the `while word == 0` check below is
//!     the lost-wakeup guard the harnesses exercise);
//!   - a send fires the persistent on-readable binding (rev1§3.6).
//!
//! Compiled only for the model/harnesses; built on `crate::sync` so the same
//! code runs under std (smoke), loom (exhaustive), and shuttle (randomized).

use crate::sync::{Arc, Condvar, Mutex};
use crate::transport::{Chan, Event, Notif, RecvErr, RecvOk, SendErr, Transport};
use std::collections::VecDeque;
use std::vec::Vec;

struct ModelMsg {
    data: Vec<u8>,
    /// Cap slots, ABI-faithful: `SLOT_NONE` (`u32::MAX`) marks an absent cap.
    caps: [u32; 4],
}

/// One channel: a bounded FIFO ring plus its event bindings (rev1§3.3, rev1§3.6).
struct Ring {
    msgs: VecDeque<ModelMsg>,
    peer_closed: bool,
    on_readable: Option<(Notif, u64)>,
    on_writable: Option<(Notif, u64)>,
    on_peer_closed: Option<(Notif, u64)>,
}

/// One notification object: a word + a condvar (rev1§3.6). The `Mutex<u64>` word is
/// the source of truth; the condvar is only the wake mechanism, so a notify
/// that races ahead of a waiter is harmless — the waiter re-checks the word.
struct Notification {
    word: Mutex<u64>,
    cv: Condvar,
}

/// A deterministic in-memory kernel: a fixed set of channels (indexed by the
/// `Chan` handle, each a bounded FIFO ring) and a fixed set of notifications.
/// Shared across model threads via `crate::sync::Arc`. The single-channel
/// harnesses drive `Chan` 0; the multi-client harness multiplexes several
/// through one reactor.
pub struct ModelTransport {
    rings: Vec<Mutex<Ring>>,
    cap: usize,
    notifs: Vec<Notification>,
}

fn fresh_ring() -> Ring {
    Ring {
        msgs: VecDeque::new(),
        peer_closed: false,
        on_readable: None,
        on_writable: None,
        on_peer_closed: None,
    }
}

impl ModelTransport {
    /// A single channel of capacity `cap` slots (rev1§3.2) and `num_notifs`
    /// notifications — the shape the single-channel harnesses use (`Chan` 0).
    pub fn new(cap: usize, num_notifs: usize) -> ModelTransport {
        ModelTransport::with_channels(1, cap, num_notifs)
    }

    /// `num_chans` channels, each of capacity `cap`, plus `num_notifs`
    /// notifications (the multi-client shape: one channel per client, one server
    /// notification multiplexing them).
    pub fn with_channels(num_chans: usize, cap: usize, num_notifs: usize) -> ModelTransport {
        let mut rings = Vec::with_capacity(num_chans);
        for _ in 0..num_chans {
            rings.push(Mutex::new(fresh_ring()));
        }
        let mut notifs = Vec::with_capacity(num_notifs);
        for _ in 0..num_notifs {
            notifs.push(Notification {
                word: Mutex::new(0),
                cv: Condvar::new(),
            });
        }
        ModelTransport { rings, cap, notifs }
    }

    /// Convenience wrapper: a fresh single-channel `ModelTransport` behind a
    /// model `Arc`.
    pub fn shared(cap: usize, num_notifs: usize) -> Arc<ModelTransport> {
        Arc::new(ModelTransport::new(cap, num_notifs))
    }

    /// Convenience wrapper: a fresh multi-channel `ModelTransport` behind a
    /// model `Arc`.
    pub fn shared_channels(num_chans: usize, cap: usize, num_notifs: usize) -> Arc<ModelTransport> {
        Arc::new(ModelTransport::with_channels(num_chans, cap, num_notifs))
    }

    fn ring(&self, ch: Chan) -> &Mutex<Ring> {
        &self.rings[ch as usize]
    }

    /// Destroy the channel (rev1§3.4): queued messages — and their caps — are
    /// **gone**, the peer is marked closed, and the on-peer-closed binding
    /// fires. Models the kernel reclaiming the channel's backing untyped (in
    /// production a cspace `cap_delete`/`revoke`, not a `Transport` op). After
    /// this, `recv_nb` on the surviving endpoint returns `Closed` — the
    /// observable "the queued cap was lost". The valuable-cap ack protocol
    /// (`Endpoint::send_acked`) exists to keep this from happening before a
    /// queued cap lands.
    pub fn destroy(&self) {
        self.destroy_chan(0);
    }

    /// Destroy a specific channel (rev1§3.4) — see [`destroy`](Self::destroy).
    pub fn destroy_chan(&self, ch: Chan) {
        let binding = {
            let mut ring = self.ring(ch).lock().unwrap();
            ring.msgs.clear();
            ring.peer_closed = true;
            ring.on_peer_closed
        };
        if let Some((n, bits)) = binding {
            self.notif_signal(n, bits);
        }
    }
}

impl Transport for ModelTransport {
    fn send_nb(&self, ch: Chan, data: &[u8], caps: Option<&[u32; 4]>) -> Result<(), SendErr> {
        let binding = {
            let mut ring = self.ring(ch).lock().unwrap();
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

    fn recv_nb(
        &self,
        ch: Chan,
        buf: &mut [u8],
        _dests: Option<&[u32; 4]>,
    ) -> Result<RecvOk, RecvErr> {
        let (result, writable) = {
            let mut ring = self.ring(ch).lock().unwrap();
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
                    // A slot freed up: fire the on-writable binding (rev1§3.3) so a
                    // sender blocked on backpressure is woken.
                    (Ok(RecvOk { len, cap_mask }), ring.on_writable)
                }
                None if ring.peer_closed => (Err(RecvErr::Closed), None),
                None => (Err(RecvErr::Empty), None),
            }
        };
        // Signal after releasing the ring lock (as send_nb does for readable).
        if let Some((n, bits)) = writable {
            self.notif_signal(n, bits);
        }
        result
    }

    fn bind(&self, ch: Chan, ev: Event, notif: Notif, bits: u64) -> Result<(), i64> {
        let mut ring = self.ring(ch).lock().unwrap();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::{thread, Arc};

    // Shuttle reproducibility ("fixed seed + replay corpus"): run every harness
    // under a *pinned* seed + iteration count
    // rather than `check_random`'s entropy seed, so each CI run explores the same
    // schedules and a failure reproduces from source — not only from the
    // schedule the failing run happens to print. Arbitrary but fixed; bump
    // deliberately (like the pinned tool versions) to widen coverage. This is
    // exactly what `shuttle::check_random` does internally, with a seeded
    // `RandomScheduler` in place of the entropy one.
    #[cfg(shuttle)]
    const SHUTTLE_SEED: u64 = 0x1C_5EED; // "ipc seed"
    #[cfg(shuttle)]
    const SHUTTLE_ITERS: usize = 1000;
    #[cfg(shuttle)]
    fn check_pinned<F: Fn() + Send + Sync + 'static>(f: F) {
        use shuttle::scheduler::RandomScheduler;
        use shuttle::Runner;
        let scheduler = RandomScheduler::new_from_seed(SHUTTLE_SEED, SHUTTLE_ITERS);
        Runner::new(scheduler, Default::default()).run(f);
    }

    // The Shuttle replay corpus (the fuzz-corpus discipline applied to
    // interleavings). When Shuttle finds a failing schedule it prints
    // an encoded replay string; paste it here as a `(harness, schedule)` entry
    // and it becomes a deterministic regression pinning that exact interleaving,
    // independent of SHUTTLE_SEED. Empty until the first bug — the designated
    // place for it to land (the empty slice keeps the `shuttle::replay` plumbing
    // type-checked). Parameterized harnesses wrap as a non-capturing fn, e.g.
    // `((|| fifo_no_drop(2)) as fn(), "…encoded…")`.
    #[cfg(shuttle)]
    #[test]
    fn shuttle_replay_corpus() {
        let corpus: &[(fn(), &str)] = &[
            // ((|| reactor_no_lost_wakeup()) as fn(), "…encoded schedule…"),
        ];
        for &(harness, schedule) in corpus {
            shuttle::replay(harness, schedule);
        }
    }

    // The rig smoke: a sender process enqueues one message (firing the
    // on-readable binding) while a receiver process binds, polls, and — if the
    // poll is Empty — waits, then receives. The message must always arrive (no
    // lost wakeup at the rig level). This is NOT the reactor — a
    // hand-inlined poll-then-wait that proves the rig is drivable by both tools.
    fn rig_smoke() {
        let t = ModelTransport::shared(2, 1);
        // Bind on-readable -> (notif 0, bit 1) before the race (the rev1§3.6 "bind
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
        check_pinned(rig_smoke);
    }

    // FIFO / no double-delivery under concurrent senders, over the *typed*
    // Endpoint. Two sender processes each send `per_sender` distinct ids on one
    // channel (A: 1..=k, B: 101..=100+k); a receiver drains all 2k. Capacity =
    // 2k, so Full never fires here — backpressure/retry is the backpressure
    // harness — and no notifications are used (waiting is the no-lost-wakeup
    // harness): pure poll.
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
        assert_eq!(
            sorted, expected,
            "every id received exactly once (no drop, no dup)"
        );

        // Per-sender FIFO: each sender's ids arrive in increasing (send) order,
        // since the channel is FIFO (rev1§3.3).
        let a: std::vec::Vec<u8> = got.iter().copied().filter(|&x| x <= per_sender).collect();
        assert!(
            a.windows(2).all(|w| w[0] < w[1]),
            "sender A not FIFO: {:?}",
            a
        );
        let b: std::vec::Vec<u8> = got.iter().copied().filter(|&x| x > 100).collect();
        assert!(
            b.windows(2).all(|w| w[0] < w[1]),
            "sender B not FIFO: {:?}",
            b
        );
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn fifo_no_drop_std() {
        fifo_no_drop(2);
    }

    // Shuttle is this harness's tier (interleaving/SC); loom is reserved for the
    // lost-wakeup fragment, so there is no loom variant here.
    #[cfg(shuttle)]
    #[test]
    fn fifo_no_drop_shuttle() {
        check_pinned(|| fifo_no_drop(2));
    }

    // No lost wakeup, over the *real* Reactor. A sender sends one message; a
    // receiver registers the
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
        assert_eq!(
            got, 42,
            "the receiver must observe the sent message (no lost wakeup)"
        );
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn reactor_no_lost_wakeup_std() {
        reactor_no_lost_wakeup();
    }

    #[cfg(shuttle)]
    #[test]
    fn reactor_no_lost_wakeup_shuttle() {
        check_pinned(reactor_no_lost_wakeup);
    }

    // The weak-memory fragment: the poll-then-wait sequence against the
    // notification word, exhaustively, at a tiny bound (one message).
    #[cfg(loom)]
    #[test]
    fn reactor_no_lost_wakeup_loom() {
        loom::model(reactor_no_lost_wakeup);
    }

    // Full backpressure + retry, no drop, over the real
    // Endpoint::send_blocking. The channel capacity is 1, so a
    // sender pushing n > 1 ids hits Full and blocks on the writable signal; the
    // receiver drains, each recv firing on_writable to wake the sender. The
    // receiver must get [1, 2, .., n] — no drop, FIFO, and the sender made
    // progress (a lost writable wakeup would deadlock). Removing recv_nb's
    // on_writable signal (model.rs) makes this hang — the negative control.
    #[cfg(not(loom))]
    fn full_backpressure_no_drop(n: u8) {
        use crate::endpoint::{Endpoint, Message};
        use crate::reactor::{Reactor, Signals};
        use crate::transport::Chan;
        const CHAN: Chan = 0;
        const NOTIF: crate::transport::Notif = 0;
        const KEY: crate::reactor::Key = 9;
        let t = ModelTransport::shared(1, 1); // capacity 1 forces backpressure

        let ts = Arc::clone(&t);
        let sender = thread::spawn(move || {
            let ep = Endpoint::new(&*ts, CHAN);
            let mut reactor = Reactor::new(&*ts, NOTIF);
            reactor.register(CHAN, Signals::WRITABLE, KEY).unwrap();
            for i in 1..=n {
                ep.send_blocking(&mut reactor, &Message::bytes(&[i]))
                    .unwrap();
            }
        });

        let tr = Arc::clone(&t);
        let receiver = thread::spawn(move || -> std::vec::Vec<u8> {
            let ep = Endpoint::new(&*tr, CHAN);
            let mut got = std::vec::Vec::new();
            let mut msg = Message::new();
            while got.len() < n as usize {
                match ep.recv_nb(&mut msg) {
                    Ok(()) => got.push(msg.payload()[0]),
                    Err(RecvErr::Empty) => thread::yield_now(),
                    Err(e) => panic!("unexpected recv error: {:?}", e),
                }
            }
            got
        });

        sender.join().unwrap();
        let got = receiver.join().unwrap();
        let expected: std::vec::Vec<u8> = (1..=n).collect();
        assert_eq!(got, expected, "no drop, FIFO, and the sender made progress");
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn full_backpressure_no_drop_std() {
        full_backpressure_no_drop(3);
    }

    // Shuttle tier (interleaving/progress); the lost-wakeup memory ordering is
    // the no-lost-wakeup harness's loom job, so there is no loom variant here.
    #[cfg(shuttle)]
    #[test]
    fn full_backpressure_no_drop_shuttle() {
        check_pinned(|| full_backpressure_no_drop(3));
    }

    // The valuable-cap ack protocol — no lost cap. The
    // sender hands off a message carrying a cap via Endpoint::send_acked, then
    // destroy()s the channel; the receiver drains via recv_acked and acks. The
    // ack gates the destroy, so the cap is received *before* destruction shreds
    // the queue — the receiver always gets it (never Closed). Removing the ack
    // gate (send_nb + immediate destroy) lets destroy race recv and lose the cap
    // (the negative control). Scope: this checks the protocol's no-loss;
    // the cap's exactly-one-owner/no-dup is the kernel's (CapRevocation).
    #[cfg(not(loom))]
    fn valuable_cap_ack_no_loss() {
        use crate::endpoint::{Endpoint, Message};
        use crate::transport::Chan;
        const CHAN: Chan = 0;
        const ACK_NOTIF: crate::transport::Notif = 0;
        const ACK_BIT: u64 = 1;
        const CAP_SLOT: u32 = 42; // the valuable cap (a cspace slot index)
        let t = ModelTransport::shared(2, 1);

        let ts = Arc::clone(&t);
        let sender = thread::spawn(move || {
            let ep = Endpoint::new(&*ts, CHAN);
            let mut msg = Message::new();
            msg.caps[0] = Some(CAP_SLOT);
            // Hand off the cap and wait for the ack before tearing down.
            ep.send_acked(&msg, ACK_NOTIF).unwrap();
            ts.destroy();
        });

        let tr = Arc::clone(&t);
        let receiver = thread::spawn(move || -> Option<u32> {
            let ep = Endpoint::new(&*tr, CHAN);
            let mut msg = Message::new();
            // Offer slot CAP_SLOT as the destination for an incoming cap.
            loop {
                msg.caps[0] = Some(CAP_SLOT);
                match ep.recv_acked(&mut msg, ACK_NOTIF, ACK_BIT) {
                    Ok(()) => return msg.caps[0],
                    Err(RecvErr::Empty) => thread::yield_now(),
                    // Closed here would mean the channel was destroyed before the
                    // cap arrived — the loss the ack protocol must prevent.
                    Err(e) => panic!("valuable cap lost: {:?}", e),
                }
            }
        });

        sender.join().unwrap();
        let got = receiver.join().unwrap();
        assert_eq!(
            got,
            Some(CAP_SLOT),
            "the receiver must land the valuable cap"
        );
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn valuable_cap_ack_no_loss_std() {
        valuable_cap_ack_no_loss();
    }

    #[cfg(shuttle)]
    #[test]
    fn valuable_cap_ack_no_loss_shuttle() {
        check_pinned(valuable_cap_ack_no_loss);
    }

    // Multi-client fairness / liveness *smoke* over the real Reactor
    // multiplexing several client channels, plus the connect/admission handshake.
    // `num_clients` client processes each fund their own request channel (chan i)
    // and reply channel (chan num_clients+i); each sends one `ConnectReq` and
    // polls its reply channel. One server process runs a single Reactor (one
    // notification) registering *every* request channel for readable, then
    // wait()-dispatches and services each connect via `admit_connect` under a
    // `budget`-bounded `Admission` — the rev1§3.5 single admission point. The
    // threads race (a connect
    // may land before or after its bind); poll-once + the notif word-check make
    // the reactor surface every client (no lost wakeup at N sources).
    //
    // Properties (schedule-independent — the granted *set* is first-come, but the
    // counts are not): every client is serviced (liveness/fairness smoke, no
    // starvation), exactly min(budget, N) are granted and the rest refused (the
    // quota never over-grants), and no reply is dropped or duplicated.
    #[cfg(not(loom))]
    fn fairness_smoke(num_clients: usize, budget: u32) {
        use crate::endpoint::{Endpoint, Message};
        use crate::reactor::{Reactor, Signals};
        use crate::session::{admit_connect, Admission, ConnectReq, GrantReply};
        use crate::transport::{Chan, Notif};
        const SERVER_NOTIF: Notif = 0;
        const WINDOW: u32 = 1; // each client requests one window byte
                               // Channel layout: request channel for client i is `i`, reply channel is
                               // `num_clients + i` (a fn, not a closure, so it crosses the move into
                               // each thread without borrowing the captured `num_clients`).
        fn req_chan(i: usize) -> Chan {
            i as Chan
        }
        fn reply_chan(n: usize, i: usize) -> Chan {
            (n + i) as Chan
        }
        // 2 channels per client (request + reply); one server notification.
        let t = ModelTransport::shared_channels(2 * num_clients, 2, 1);

        let ts = Arc::clone(&t);
        let server = thread::spawn(move || -> std::vec::Vec<GrantReply> {
            let mut adm = Admission::new(budget);
            let mut reactor = Reactor::new(&*ts, SERVER_NOTIF);
            for i in 0..num_clients {
                reactor.register(req_chan(i), Signals::READABLE, i).unwrap();
            }
            let mut replies: std::vec::Vec<Option<GrantReply>> = std::vec![None; num_clients];
            let mut served = 0;
            let mut msg = Message::new();
            while served < num_clients {
                let (key, _signals) = reactor.wait();
                let req_ep = Endpoint::new(&*ts, req_chan(key));
                let reply_ep = Endpoint::new(&*ts, reply_chan(num_clients, key));
                // Drain this source (a wakeup is level-ish — poll until Empty).
                loop {
                    match req_ep.recv_nb(&mut msg) {
                        Ok(()) => {
                            assert!(replies[key].is_none(), "client {key} serviced twice");
                            let reply = admit_connect(&mut adm, msg.payload());
                            let (bytes, n) = reply.encode();
                            reply_ep.send_nb(&Message::bytes(&bytes[..n])).unwrap();
                            replies[key] = Some(reply);
                            served += 1;
                        }
                        Err(RecvErr::Empty) => break,
                        Err(e) => panic!("server recv error: {:?}", e),
                    }
                }
            }
            replies.into_iter().map(|r| r.unwrap()).collect()
        });

        let mut clients = std::vec::Vec::new();
        for i in 0..num_clients {
            let tc = Arc::clone(&t);
            clients.push(thread::spawn(move || -> GrantReply {
                let req_ep = Endpoint::new(&*tc, req_chan(i));
                let reply_ep = Endpoint::new(&*tc, reply_chan(num_clients, i));
                req_ep
                    .send_nb(&Message::bytes(&ConnectReq::for_window(WINDOW).encode()))
                    .unwrap();
                let mut msg = Message::new();
                loop {
                    match reply_ep.recv_nb(&mut msg) {
                        Ok(()) => {
                            return GrantReply::decode(msg.payload())
                                .expect("server reply must decode");
                        }
                        Err(RecvErr::Empty) => thread::yield_now(),
                        Err(e) => panic!("client {i} recv error: {:?}", e),
                    }
                }
            }));
        }

        let server_view = server.join().unwrap();
        let client_views: std::vec::Vec<GrantReply> =
            clients.into_iter().map(|c| c.join().unwrap()).collect();

        // Every client got a reply (liveness/fairness smoke: none starved).
        assert_eq!(client_views.len(), num_clients);
        // The server's record of each client matches what that client received
        // (no drop / no cross-wire / no duplication across the N channels).
        assert_eq!(
            server_view, client_views,
            "server/client reply views diverged"
        );
        // The quota never over-grants: exactly min(budget, N) grants.
        let grants = client_views
            .iter()
            .filter(|r| matches!(r, GrantReply::Grant(_)))
            .count();
        let refused = client_views
            .iter()
            .filter(|r| matches!(r, GrantReply::Refused))
            .count();
        let expect_grants = (budget as usize).min(num_clients);
        assert_eq!(grants, expect_grants, "wrong number of admissions");
        assert_eq!(
            refused,
            num_clients - expect_grants,
            "wrong number of refusals"
        );
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn fairness_smoke_std() {
        fairness_smoke(3, 3); // admit all
        fairness_smoke(3, 2); // quota refuses one
    }

    // Shuttle is this harness's tier (interleaving / progress at scale); the
    // lost-wakeup memory ordering stays the no-lost-wakeup harness's loom job, so
    // there is no loom variant.
    #[cfg(shuttle)]
    #[test]
    fn fairness_smoke_shuttle() {
        check_pinned(|| fairness_smoke(3, 2));
    }

    // `register_bound` dispatch — the rev1§5.1 thread-source path the shell's
    // spawn/reap loop uses (`user/shell/src/main.rs`): two externally-bound,
    // edge-triggered sources on one notification, each dispatched to its key.
    // Unlike `register`, `register_bound` does NOT poll-once; the test proves it
    // by registering two bits and signaling only the *higher* one, then asserting
    // `wait` returns *its* key — a fabricated poll-once would have left the low
    // bit pending and (lowest-first) returned it instead. Also checks that a
    // re-register of a used bit is `Taken`. Std-only: a sequential check of the
    // dispatch logic, not a concurrency harness.
    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn reactor_register_bound_dispatch() {
        use crate::reactor::{Key, Reactor, RegisterErr};
        use crate::transport::Notif;
        const NOTIF: Notif = 0;
        const BIT_LO: u64 = 1 << 0;
        const BIT_HI: u64 = 1 << 1;
        const KEY_LO: Key = 100;
        const KEY_HI: Key = 200;
        let t = ModelTransport::shared(1, 1);
        let mut reactor = Reactor::new(&*t, NOTIF);
        reactor.register_bound(BIT_LO, KEY_LO).unwrap();
        reactor.register_bound(BIT_HI, KEY_HI).unwrap();
        // A used bit cannot be re-registered.
        assert_eq!(reactor.register_bound(BIT_LO, 999), Err(RegisterErr::Taken));
        // No poll-once: nothing is pending until a source actually fires. Signal
        // only the high bit (as a thread death would) and assert wait returns its
        // key — the low bit was not self-signaled at registration time.
        t.notif_signal(NOTIF, BIT_HI);
        assert_eq!(reactor.wait().0, KEY_HI);
        // The low source then fires and dispatches to its own key.
        t.notif_signal(NOTIF, BIT_LO);
        assert_eq!(reactor.wait().0, KEY_LO);
    }
}
