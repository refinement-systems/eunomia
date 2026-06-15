//! Notification objects (spec §3.6): a machine word of signal bits plus a
//! FIFO waiter queue. Signalers OR bits in; a waiter receives the whole
//! accumulated word, which clears. Event delivery never allocates — the
//! waiter queue is intrusive through the TCBs.

use crate::cspace::{self, NotifView, ObjHeader};
use crate::id::ObjId;
use crate::store::Store;
use crate::thread::ThreadState;
use vstd::prelude::*;
// `StoreSpec` (the `external_trait_extension`) must be in scope to resolve
// `store.notif_view()`/`tcb_view()`/… in the §4b contracts; it erases in a
// normal build, so it is otherwise unused here (the doc/results/26 §2.3 idiom).
#[allow(unused_imports)]
use crate::cspace::StoreSpec;

#[repr(C)]
pub struct NotifObj {
    pub hdr: ObjHeader,
    pub word: u64,
    pub wait_head: Option<ObjId>,
    pub wait_tail: Option<ObjId>,
}

impl NotifObj {
    /// pre:  memory at `this` writable.
    /// post: clear word, no waiters, refs = 1 (creator cap).
    ///
    /// Production construction/layout helper: places the object at a
    /// caller-supplied pointer. Not part of the Store-based core.
    pub unsafe fn init(this: *mut NotifObj) {
        this.write(NotifObj {
            hdr: ObjHeader { refs: 1 },
            word: 0,
            wait_head: None,
            wait_tail: None,
        });
    }
}

verus! {

/// `signal` *wakes* (delivers the word + dequeues the head waiter) iff after the OR
/// the word is nonzero AND a waiter is queued; otherwise it *accumulates* the word.
pub open spec fn signal_wakes(nv: Map<ObjId, NotifView>, n: ObjId, bits: u64) -> bool {
    (nv[n].word | bits) != 0 && nv[n].wait_head is Some
}

/// OR bits into the word and deliver to the first waiter, if any.
/// Safe from any kernel context (syscall, timer IRQ, channel binding).
///
/// Verified (plan §4b, doc/results/32): graduates from the phase-3 assumed
/// `external_body` frame (doc 27 §1) to a proven body against the `waiter_seq` FIFO
/// model. The `slot_view`/`chan_view`-unchanged frame is *retained* (so the phase-3
/// callers `fire`/`send`/`recv`/`endpoint_cap_dropped` keep using `signal`'s result
/// unchanged — the strengthening is additive on the `ensures` side); the new
/// preconditions (the notification is live + `notif_wf`, and a queued waiter implies
/// `refs > 0`) are discharged by the channel ops via `cspace::binding_notif_wf`. On
/// the **wake** path the head waiter is dequeued (`waiter_seq` loses its head —
/// `Seq::drop_first`), receives the whole accumulated word, the word clears, the
/// queued ref is released (`refs -= 1`), and the thread is made Runnable; on the
/// **accumulate** path the word grows and the queue/refs are untouched. `notif_wf`
/// is preserved either way.
pub fn signal<S: Store>(store: &mut S, n: ObjId, bits: u64)
    requires
        old(store).notif_view().dom().contains(n),
        cspace::notif_wf(old(store).notif_view(), old(store).tcb_view(), n),
        old(store).notif_view()[n].wait_head is Some
            ==> old(store).refs_view().dom().contains(n) && old(store).refs_view()[n] > 0,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        // The whole effect is confined to notification `n` (in `notif_view`) and the
        // one woken thread (in `tcb_view`); the domains never move.
        final(store).notif_view() == old(store).notif_view().insert(n, final(store).notif_view()[n]),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        // `signal` touches only TCBs that were waiting on `n` (just the woken head, or
        // none) — the frame a caller needs to keep *other* notifications' `notif_wf`
        // intact across a fire (`cspace::lemma_notif_wf_frame`).
        forall|k: ObjId| #[trigger] old(store).tcb_view()[k].wait_notif != Some(n)
            ==> final(store).tcb_view()[k] == old(store).tcb_view()[k],
        cspace::notif_wf(final(store).notif_view(), final(store).tcb_view(), n),
        // Accumulate path: word grows, queue + refs + TCBs untouched.
        !signal_wakes(old(store).notif_view(), n, bits) ==> {
            &&& final(store).notif_view()[n].word == (old(store).notif_view()[n].word | bits)
            &&& final(store).tcb_view() == old(store).tcb_view()
            &&& final(store).refs_view() == old(store).refs_view()
            &&& cspace::waiter_seq(final(store).notif_view(), final(store).tcb_view(), n)
                    == cspace::waiter_seq(old(store).notif_view(), old(store).tcb_view(), n)
        },
        // Wake path: head dequeued, delivered, word cleared, queued ref released.
        signal_wakes(old(store).notif_view(), n, bits) ==> {
            let t = old(store).notif_view()[n].wait_head->Some_0;
            &&& old(store).tcb_view()[t].wait_notif == Some(n)
            &&& final(store).tcb_view() == old(store).tcb_view().insert(t, final(store).tcb_view()[t])
            &&& final(store).tcb_view()[t].state == ThreadState::Runnable
            &&& final(store).tcb_view()[t].retval == (old(store).notif_view()[n].word | bits)
            &&& final(store).notif_view()[n].word == 0
            &&& final(store).refs_view()
                    == old(store).refs_view().insert(n, (old(store).refs_view()[n] - 1) as nat)
            &&& cspace::waiter_seq(final(store).notif_view(), final(store).tcb_view(), n)
                    == cspace::waiter_seq(old(store).notif_view(), old(store).tcb_view(), n).drop_first()
        },
{
    let ghost nv0 = old(store).notif_view();
    let ghost tv0 = old(store).tcb_view();
    let ghost ws0 = cspace::waiter_seq(nv0, tv0, n);
    // `notif_wf` gives `exists ws. waiter_chain`; `waiter_seq` is the `choose`, so the
    // chosen `ws0` is itself a witness.
    assert(cspace::waiter_chain(nv0, tv0, n, ws0));

    let word = store.notif_word(n) | bits;
    store.set_notif_word(n, word);
    let head = store.notif_wait_head(n);
    if word == 0 || head.is_none() {
        // Accumulate path. Only `nv[n].word` changed; the chain `ws0` still threads and
        // no TCB moved at all.
        proof {
            assert forall|k: ObjId| old(store).tcb_view()[k].wait_notif != Some(n) implies
                #[trigger] store.tcb_view()[k] == old(store).tcb_view()[k] by {}
            assert(cspace::waiter_chain(store.notif_view(), store.tcb_view(), n, ws0));
            cspace::lemma_waiter_chain_unique(
                store.notif_view(), store.tcb_view(), n,
                cspace::waiter_seq(store.notif_view(), store.tcb_view(), n), ws0);
        }
        return;
    }

    // Wake path. `wait_head is Some` ⇒ `ws0` is non-empty and its head is `t`.
    let t = head.unwrap();
    assert(ws0.len() > 0);
    assert(nv0[n].wait_head == Some(ws0[0]));
    assert(ws0[0] == t);
    let next = store.tcb_qnext(t);
    assert(next == tv0[t].qnext);

    store.set_notif_wait_head(n, next);
    if next.is_none() {
        store.set_notif_wait_tail(n, None);
    }
    store.set_tcb_qnext(t, None);
    store.set_tcb_wait_notif(t, None);
    store.set_tcb_retval(t, word);
    store.set_notif_word(n, 0);
    // The waiter held a ref while queued (it has no cap in hand mid-wait); release it.
    store.set_obj_refs(n, store.obj_refs(n) - 1);
    store.make_runnable(t);

    proof {
        let dws = ws0.drop_first();
        let nvf = store.notif_view();
        let tvf = store.tcb_view();
        // The imperative fixups touched only `t` (its old `wait_notif` was `Some(n)`, the
        // head) and re-pointed `n`'s head/tail past it.
        assert(tv0[t].wait_notif == Some(n));
        assert(tv0.dom().contains(ws0[0]));
        assert(tvf.dom() == tv0.dom());
        assert forall|k: ObjId| #![trigger tvf[k]] k != t implies tvf[k] == tv0[k] by {}
        assert forall|k: ObjId| old(store).tcb_view()[k].wait_notif != Some(n) implies
            #[trigger] tvf[k] == old(store).tcb_view()[k] by {}
        cspace::lemma_drop_first_chain(nv0, tv0, nvf, tvf, n, t, ws0);
        cspace::lemma_waiter_chain_unique(nvf, tvf, n,
            cspace::waiter_seq(nvf, tvf, n), dws);
    }
}

/// Wait: consume the word if nonzero, else block the current thread.
///
/// Verified (plan §4b): on a nonzero word the thread returns it and the word clears
/// (the queue/refs are untouched); otherwise `cur` is appended at the tail
/// (`waiter_seq` grows by `Seq::push` — the FIFO/block-order half of the wake-order
/// theorem), is marked `BlockedNotif`/`wait_notif = Some(n)`, and acquires one ref on
/// `n` (released on wake or teardown). `notif_wf` is preserved. The `cur` thread must
/// not already be waiting on `n` (`wait_notif != Some(n)`), so the push keeps the
/// chain duplicate-free.
pub fn wait<S: Store>(store: &mut S, n: ObjId, cur: ObjId) -> (res: Option<u64>)
    requires
        old(store).notif_view().dom().contains(n),
        old(store).tcb_view().dom().contains(cur),
        cspace::notif_wf(old(store).notif_view(), old(store).tcb_view(), n),
        old(store).tcb_view()[cur].wait_notif != Some(n),
        old(store).notif_view()[n].word == 0
            ==> old(store).refs_view().dom().contains(n) && old(store).refs_view()[n] < u32::MAX,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view().dom() == old(store).notif_view().dom(),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        cspace::notif_wf(final(store).notif_view(), final(store).tcb_view(), n),
        // Consume path: word returned and cleared, queue + refs + TCBs untouched.
        old(store).notif_view()[n].word != 0 ==> {
            &&& res == Some(old(store).notif_view()[n].word)
            &&& final(store).notif_view()[n].word == 0
            &&& final(store).tcb_view() == old(store).tcb_view()
            &&& final(store).refs_view() == old(store).refs_view()
            &&& cspace::waiter_seq(final(store).notif_view(), final(store).tcb_view(), n)
                    == cspace::waiter_seq(old(store).notif_view(), old(store).tcb_view(), n)
        },
        // Block path: `cur` appended FIFO, marked blocked, one ref acquired.
        old(store).notif_view()[n].word == 0 ==> {
            &&& res is None
            &&& final(store).tcb_view()[cur].state == ThreadState::BlockedNotif
            &&& final(store).tcb_view()[cur].wait_notif == Some(n)
            &&& final(store).refs_view()
                    == old(store).refs_view().insert(n, (old(store).refs_view()[n] + 1) as nat)
            &&& cspace::waiter_seq(final(store).notif_view(), final(store).tcb_view(), n)
                    == cspace::waiter_seq(old(store).notif_view(), old(store).tcb_view(), n).push(cur)
        },
{
    let ghost nv0 = old(store).notif_view();
    let ghost tv0 = old(store).tcb_view();
    let ghost ws0 = cspace::waiter_seq(nv0, tv0, n);
    assert(cspace::waiter_chain(nv0, tv0, n, ws0));

    let word = store.notif_word(n);
    if word != 0 {
        store.set_notif_word(n, 0);
        // Consume: only `nv[n].word` changed; `ws0` still threads.
        proof {
            assert(cspace::waiter_chain(store.notif_view(), store.tcb_view(), n, ws0));
            cspace::lemma_waiter_chain_unique(
                store.notif_view(), store.tcb_view(), n,
                cspace::waiter_seq(store.notif_view(), store.tcb_view(), n), ws0);
        }
        return Some(word);
    }

    store.set_tcb_state(cur, ThreadState::BlockedNotif);
    store.set_tcb_wait_notif(cur, Some(n));
    store.set_tcb_qnext(cur, None);
    match store.notif_wait_tail(n) {
        None => store.set_notif_wait_head(n, Some(cur)),
        Some(tail) => store.set_tcb_qnext(tail, Some(cur)),
    }
    store.set_notif_wait_tail(n, Some(cur));
    store.set_obj_refs(n, store.obj_refs(n) + 1);

    proof {
        let pws = ws0.push(cur);
        let nvf = store.notif_view();
        let tvf = store.tcb_view();
        // `cur ∉ ws0`: every chain node has `wait_notif == Some(n)`, but `cur`'s old
        // `wait_notif != Some(n)` (precondition).
        assert(forall|i: int| 0 <= i < ws0.len() ==> #[trigger] tv0[ws0[i]].wait_notif == Some(n));
        assert(forall|i: int| 0 <= i < ws0.len() ==> #[trigger] ws0[i] != cur);
        assert(pws.no_duplicates()) by {
            assert forall|i: int, j: int|
                0 <= i < pws.len() && 0 <= j < pws.len() && i != j implies pws[i] != pws[j] by {
                if i < ws0.len() && j < ws0.len() {
                    assert(pws[i] == ws0[i] && pws[j] == ws0[j]);
                } else if i < ws0.len() {
                    assert(pws[i] == ws0[i] && pws[j] == cur);
                } else if j < ws0.len() {
                    assert(pws[j] == ws0[j] && pws[i] == cur);
                }
            }
        }
        assert(cspace::waiter_chain(nvf, tvf, n, pws)) by {
            assert forall|i: int| 0 <= i < pws.len() implies #[trigger] tvf.dom().contains(pws[i]) by {
                if i < ws0.len() { assert(pws[i] == ws0[i]); } else { assert(pws[i] == cur); }
            }
            assert forall|i: int| 0 <= i < pws.len() implies
                tvf[pws[i]].qnext == (if i + 1 < pws.len() { Some(pws[i + 1]) } else { None }) by {
                if i + 1 < ws0.len() {
                    assert(pws[i] == ws0[i] && ws0[i] != cur);
                    assert(tv0[ws0[i]].qnext == Some(ws0[i + 1]));
                } else if i + 1 == ws0.len() {
                    // `ws0`'s last node: its qnext was retargeted to `cur`.
                    assert(pws[i] == ws0[i] && pws[i + 1] == cur);
                } else {
                    // `cur` itself: qnext stays None.
                    assert(pws[i] == cur);
                }
            }
            assert forall|i: int| 0 <= i < pws.len() implies
                tvf[pws[i]].wait_notif == Some(n)
                && tvf[pws[i]].state == ThreadState::BlockedNotif by {
                if i < ws0.len() {
                    assert(pws[i] == ws0[i] && ws0[i] != cur);
                } else {
                    assert(pws[i] == cur);
                }
            }
        }
        cspace::lemma_waiter_chain_unique(nvf, tvf, n,
            cspace::waiter_seq(nvf, tvf, n), pws);
    }
    None
}

/// pre:  refs == 0 — no caps, no bindings, no armed timers, no waiters.
///
/// Verified (plan §4b): a no-op. The no-waiters condition is supplied directly
/// (`wait_head is None`) — exactly what the production `debug_assert` checks. The
/// "`refs == 0` ⇒ no waiters" justification (a waiter holds a ref) is the refcount
/// census deferred to the post-phase-5 teardown phase (plan §1.4), so it is *not*
/// derivable from the structural `notif_wf` here; requiring the empty queue is the
/// honest scoped contract (doc/results/32).
pub fn destroy_notif<S: Store>(store: &mut S, n: ObjId)
    requires
        old(store).notif_view().dom().contains(n),
        old(store).notif_view()[n].wait_head is None,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).refs_view() == old(store).refs_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
{
    let _ = n;
}

} // verus!

/// Unlink a waiter (thread teardown path). Phase 4c verifies this body; today it is
/// plain Rust outside the verified block.
pub fn remove_waiter<S: Store>(store: &mut S, n: ObjId, t: ObjId) {
    let mut cur = store.notif_wait_head(n);
    let mut prev: Option<ObjId> = None;
    while let Some(c) = cur {
        if c == t {
            let next = store.tcb_qnext(c);
            match prev {
                None => store.set_notif_wait_head(n, next),
                Some(p) => store.set_tcb_qnext(p, next),
            }
            if store.notif_wait_tail(n) == Some(t) {
                store.set_notif_wait_tail(n, prev);
            }
            store.set_tcb_qnext(t, None);
            store.set_tcb_wait_notif(t, None);
            store.set_obj_refs(n, store.obj_refs(n) - 1);
            return;
        }
        prev = cur;
        cur = store.tcb_qnext(c);
    }
}
