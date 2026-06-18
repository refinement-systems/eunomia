//! Notification objects (spec §3.6): a machine word of signal bits plus a
//! FIFO waiter queue. Signalers OR bits in; a waiter receives the whole
//! accumulated word, which clears. Event delivery never allocates — the
//! waiter queue is intrusive through the TCBs.

// `cspace::`/`NotifView` are referenced only from `verus!{}` spec/proof code,
// which erases under a normal build — hence the allow (the lib.rs precedent).
#[allow(unused_imports)]
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
        // `signal` touches no cspace residency (every setter frames it) — the frame
        // `lemma_caps_consistent_frame` (via `fire`) needs (plan §6d body PR).
        final(store).cspace_view() == old(store).cspace_view(),
        // The refcount census moves in lockstep (plan §6d body PR): a wake drops `refs[n]`
        // and `waiter_seq(n)` together, so `refs[x] - census(x)` is frozen at every `x`.
        // Unconditional and `requires`-free, so the kernel-shell callers
        // `report_terminal`/`check_expired` and the construction-op callers of `fire`
        // (`send`/`recv`) are undisturbed; `delete` consumes it across the off-by-one window.
        cspace::census_delta_frozen(old(store), final(store)),
        // `refcount_sound` as a *system* invariant (plan §6f): a sound census in, a sound
        // census out. Conditional, so census-agnostic callers (`check_expired`'s
        // `signal`-in-a-loop, `report_terminal`) stay undisturbed; it is the frozen delta
        // (above) bridged by `lemma_refcount_sound_from_frozen`.
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
        // A census off by one at any `z` survives the wake (it is the frozen delta applied to
        // that shape) — `delete`'s Channel branch reads this off the fire chain to carry the
        // deleted-slot off-by-one across the peer-closed fire. The trigger keeps it out of
        // census-agnostic callers (`check_expired`'s `signal`-in-a-loop).
        forall|z: ObjId| cspace::census_off_by_one(old(store), z)
            ==> #[trigger] cspace::census_off_by_one(final(store), z),
        // Refs-domain completeness survives the wake (the census only drops, the refs domain
        // is unchanged) — `delete`'s Channel branch carries it across the fire to `obj_unref`.
        // Conditional + obj_census-triggered, so `check_expired` is undisturbed (doc 50).
        cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(final(store)),
        // Dead, queue-detached TCBs are frozen across the wake (plan §6d-final-thread-body): a
        // signal touches only the woken head (`wait_notif == Some(n)`) and drops `refs[n]` (which
        // a waiter held, so `refs[n] > 0`), so a `wait_notif is None`, `refs == 0` object `x` is
        // not the head (`x != n`) and is untouched. `fire`/`endpoint_cap_dropped`/`delete` carry
        // it up the teardown chain.
        cspace::dead_tcb_frozen(old(store), final(store)),
        // The timer views are untouched (plan §4d): every setter in the body frames
        // them and `make_runnable` frames them, so `report_terminal` (which fires
        // `signal` and otherwise touches no timer) can frame timers across the wake.
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
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
            // The woken thread's binding slots are untouched — `signal` moves only its
            // queue/wait/retval fields. The frame `caps_consistent` preservation needs (a
            // Thread cap for `t` reads its `bind_slots`; plan §6d body PR).
            &&& final(store).tcb_view()[t].bind_slots == old(store).tcb_view()[t].bind_slots
            // …and its bound cspace/aspace are untouched too — the strengthened
            // `cap_consistent(Thread)` clause reads `tcb[t].cspace` (its `cspace_resident_wf`),
            // so `lemma_caps_consistent_frame` needs the cspace frame across the wake; the
            // aspace half rides the same proof (`thread_hold_refs`). Plan §6d-final-thread.
            &&& final(store).tcb_view()[t].cspace == old(store).tcb_view()[t].cspace
            &&& final(store).tcb_view()[t].aspace == old(store).tcb_view()[t].aspace
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
            // census_delta_frozen: refs untouched; the census is framed — only `nv[n].word`
            // moved (not a census term), `tv` is identical, and the slot/chan/timer views are
            // framed, so `waiter_refs(o)` rides through for every `o` (it reads `nv` only at
            // `o`, framed for `o != n`; the chain at `n` is `ws0` either way). With refs and
            // census both unchanged, the delta is trivially frozen.
            assert forall|o: ObjId| #[trigger] cspace::obj_census(store, o)
                == cspace::obj_census(old(store), o) by {
                if o != n {
                    cspace::lemma_waiter_refs_frame_nv(
                        old(store).notif_view(), store.notif_view(), store.tcb_view(), o);
                }
            }
            assert(cspace::census_delta_frozen(old(store), store));
            // refcount_sound (conditional, plan §6f): the frozen delta bridges it.
            if cspace::refcount_sound(old(store)) {
                cspace::lemma_refcount_sound_from_frozen(old(store), store);
            }
            assert forall|z: ObjId| cspace::census_off_by_one(old(store), z) implies
                #[trigger] cspace::census_off_by_one(store, z) by {
                cspace::lemma_off_by_one_frozen(old(store), store, z);
            }
            // census_dom_complete: census + refs domain both unchanged ⇒ coverage carries.
            if cspace::census_dom_complete(old(store)) {
                assert forall|o: ObjId| #[trigger] cspace::obj_census(store, o) >= 1 implies
                    store.refs_view().dom().contains(o) by {}
            }
            // dead_tcb_frozen: the accumulate path moved no TCB and no ref, so every dead,
            // detached object is trivially frozen (signal-shaped with no waiter moved).
            assert(store.refs_view() == old(store).refs_view());
            assert forall|k: ObjId| #[trigger] store.tcb_view()[k] == old(store).tcb_view()[k]
                || old(store).tcb_view()[k].wait_notif == Some(n) by {}
            assert(store.refs_view().dom() =~= old(store).refs_view().dom());
            assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
            cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
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
        // refcount_sound (conditional): `refs[n]` dropped by one, matched by `waiter_refs(n)`
        // losing the woken head (`waiter_seq(n) == ws0.drop_first()`); every other object's
        // census is framed. The slot/chan/timer terms ride the view frames; `thread_hold`
        // rides `lemma_thread_hold_frame` (only `t`'s queue/wait fields moved, never cspace/
        // aspace); `waiter_refs(o)` for `o != n` rides `lemma_waiter_refs_frame`.
        assert(tvf[t].cspace == tv0[t].cspace);
        assert(tvf[t].aspace == tv0[t].aspace);
        assert forall|k: ObjId| #[trigger] tvf[k].cspace == tv0[k].cspace by {
            if k != t {}
        }
        assert forall|k: ObjId| #[trigger] tvf[k].aspace == tv0[k].aspace by {
            if k != t {}
        }
        assert(tvf[t].wait_notif is None);
        // census_delta_frozen: `refs[n]` dropped by one, matched by `waiter_refs(n)` losing
        // the woken head (`waiter_seq(n) == ws0.drop_first()`, one shorter); every other
        // object's census is framed (slot/chan/timer view frames; `thread_hold` via
        // `lemma_thread_hold_frame`; `waiter_refs(o)` for `o != n` via `lemma_waiter_refs_frame`).
        // So `refs[x] - census(x)` is unchanged at every `x`.
        assert(dws.len() == ws0.len() - 1);
        assert(cspace::waiter_refs(nv0, tv0, n) == ws0.len());
        assert(cspace::waiter_refs(nvf, tvf, n) == dws.len());
        assert(store.refs_view().dom() == old(store).refs_view().dom());
        assert(store.refs_view().dom().contains(n));
        // The census delta over **every** object (not just the refs domain): it drops by one
        // at the woken `n` (the dequeued waiter) and is framed elsewhere (`thread_hold` via
        // `lemma_thread_hold_frame`; `waiter_refs(o)` for `o != n` via `lemma_waiter_refs_frame`;
        // slot/chan/timer by the view frames). `census_delta_frozen`, `census_off_by_one`
        // preservation, and `census_dom_complete` preservation all derive from this.
        assert forall|o: ObjId| #[trigger] cspace::obj_census(store, o)
            == (if o == n { (cspace::obj_census(old(store), n) - 1) as nat } else {
                cspace::obj_census(old(store), o)
            }) by {
            cspace::lemma_thread_hold_frame(tv0, tvf, o);
            if o != n {
                assert forall|k: ObjId| #[trigger] tvf[k] != tv0[k]
                    implies tv0[k].wait_notif != Some(o) && tvf[k].wait_notif != Some(o) by {
                    if tvf[k] != tv0[k] {
                        assert(k == t);
                    }
                }
                cspace::lemma_waiter_refs_frame(nv0, tv0, nvf, tvf, n, o);
            }
        }
        assert(cspace::census_delta_frozen(old(store), store));
        // refcount_sound (conditional, plan §6f): the frozen delta bridges it.
        if cspace::refcount_sound(old(store)) {
            cspace::lemma_refcount_sound_from_frozen(old(store), store);
        }
        assert forall|z: ObjId| cspace::census_off_by_one(old(store), z) implies
            #[trigger] cspace::census_off_by_one(store, z) by {
            cspace::lemma_off_by_one_frozen(old(store), store, z);
        }
        if cspace::census_dom_complete(old(store)) {
            assert forall|o: ObjId| #[trigger] cspace::obj_census(store, o) >= 1 implies
                store.refs_view().dom().contains(o) by {
                // census(store, o) <= census(old, o), so a positive-census o had one before ⇒
                // it was covered; the domain is unchanged.
                assert(cspace::obj_census(old(store), o) >= 1);
            }
        }
        // dead_tcb_frozen: a signal-shaped edit. Only the woken head `t` (`wait_notif == Some(n)`)
        // moved; `refs` dropped only at `n`, which had `refs > 0` (the woken waiter held it), so a
        // dead (`refs 0`), detached (`wait_notif is None`) `x` is neither `t` nor `n` and is frozen.
        assert(old(store).refs_view()[n] > 0);
        assert(store.refs_view()
            == old(store).refs_view().insert(n, (old(store).refs_view()[n] - 1) as nat));
        assert forall|x: ObjId|
            old(store).refs_view().dom().contains(x) && old(store).refs_view()[x] == 0
            implies #[trigger] store.refs_view()[x] == 0 by {
            assert(x != n);
        }
        assert forall|k: ObjId| #[trigger] store.tcb_view()[k] == old(store).tcb_view()[k]
            || old(store).tcb_view()[k].wait_notif == Some(n) by {}
        assert(store.refs_view().dom() =~= old(store).refs_view().dom());
        assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
        cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
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
        cspace::caps_consistent(old(store)),
        cspace::end_caps_sound(old(store)),
        cspace::census_dom_complete(old(store)),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).refs_view() == old(store).refs_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        // A model no-op (the kernel reclaims the object's memory; the abstract views are
        // untouched), so the cap→object invariant rides through trivially (plan §6d).
        cspace::caps_consistent(final(store)),
        cspace::end_caps_sound(final(store)),
        cspace::census_dom_complete(final(store)),
{
    let _ = n;
    let _ = store;
}

/// Unlink waiter `t` from notification `n`'s queue (the thread-teardown path).
///
/// Verified (plan §4c, doc/results/33): the mid-queue unlink — the `cdt_unlink` analog
/// (doc 25) but singly-linked with no re-parenting, so the removal is a plain `Seq`
/// splice. If `t` is queued on `n` it is spliced out (`waiter_seq(n)` loses exactly the
/// `t` element, the FIFO order of the rest preserved — `Seq::remove`), its `qnext`/
/// `wait_notif` are cleared, and the queued ref is released (`refs[n] -= 1`, the second
/// installment of `refcount_sound`'s waiter term after `signal`'s pop-release); if `t`
/// is absent the store is unchanged. `notif_wf(n)` is preserved either way. The walk is
/// read-only — the only writes are on the found path, which returns. The `refs > 0`
/// precondition (a non-empty queue ⇒ live) discharges the release `-1`, exactly as in
/// `signal`.
pub fn remove_waiter<S: Store>(store: &mut S, n: ObjId, t: ObjId)
    requires
        old(store).notif_view().dom().contains(n),
        cspace::notif_wf(old(store).notif_view(), old(store).tcb_view(), n),
        old(store).notif_view()[n].wait_head is Some
            ==> old(store).refs_view().dom().contains(n) && old(store).refs_view()[n] > 0,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).notif_view().dom() == old(store).notif_view().dom(),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        // Every TCB's immutable `bind_slots` survive the splice (plan §6e): a signal-shaped edit
        // writes only queue/wait links, never `bind_slots`. `destroy_tcb` reads it off for the
        // `home_views_frozen` stability across its BlockedNotif detach.
        forall|k: ObjId| #[trigger] final(store).tcb_view()[k].bind_slots
            == old(store).tcb_view()[k].bind_slots,
        cspace::notif_wf(final(store).notif_view(), final(store).tcb_view(), n),
        // The refcount census moves in lockstep (plan §6d body PR, doc 45 §3): the splice
        // drops `refs[n]` and `waiter_seq(n)` (losing `t`) together; absent, nothing moves.
        // Unconditional — `destroy_tcb` turns it into `refcount_sound` via
        // `lemma_refcount_sound_from_frozen` (it calls `remove_waiter` where the census is sound).
        cspace::census_delta_frozen(old(store), final(store)),
        // `refcount_sound` as a system invariant (plan §6f): the frozen delta bridges a sound
        // census in to a sound census out. Conditional + `requires`-free, so the phase-4 callers
        // keep no obligation; `destroy_tcb` already consumes the frozen delta directly.
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
        // Residency is untouched (the splice writes notif head/tail + tcb queue links + `refs`,
        // never `cspace_view`); `destroy_tcb` carries it to its own `cspace_view` ensures (plan
        // §6d-final-thread).
        final(store).cspace_view() == old(store).cspace_view(),
        // The teardown system invariants survive the splice (the `signal`→`fire` precedent):
        // it is a signal-shaped edit (only `n`'s notif view + `n`'s waiter TCBs move, every
        // TCB's `bind_slots`/`cspace` fixed), so `lemma_caps_consistent_frame` applies; the
        // §3.3 endpoint census reads only the framed chan/slot views; and the census only
        // drops while the refs domain is fixed. Conditional + `requires`-free, so the phase-4
        // callers keep no obligation; `destroy_tcb` (the only kcore caller) consumes them for
        // its bind-slot `delete`s (plan §6d-final-thread).
        cspace::caps_consistent(old(store)) ==> cspace::caps_consistent(final(store)),
        cspace::end_caps_sound(old(store)) ==> cspace::end_caps_sound(final(store)),
        cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(final(store)),
        ({
            let ws0 = cspace::waiter_seq(old(store).notif_view(), old(store).tcb_view(), n);
            // Absent: `t` not on `n`'s queue ⇒ the store is unchanged.
            &&& !ws0.contains(t) ==> {
                    &&& final(store).notif_view() == old(store).notif_view()
                    &&& final(store).tcb_view() == old(store).tcb_view()
                    &&& final(store).refs_view() == old(store).refs_view()
                }
            // Present: `t` spliced out (FIFO order of the rest preserved), its links
            // cleared, the queued ref released.
            &&& ws0.contains(t) ==> {
                    &&& cspace::waiter_seq(final(store).notif_view(), final(store).tcb_view(), n)
                            == ws0.remove(ws0.index_of(t))
                    &&& final(store).tcb_view()[t].qnext is None
                    &&& final(store).tcb_view()[t].wait_notif is None
                    &&& final(store).refs_view()
                            == old(store).refs_view().insert(n, (old(store).refs_view()[n] - 1) as nat)
                    // The splice writes only `t`'s queue links (`qnext`/`wait_notif`); `t`'s
                    // every *other* field survives. `destroy_tcb` (the only kcore caller) reads
                    // this off across the BlockedNotif detach: it needs `t`'s `cspace`/`aspace`
                    // (to drive `unref_cspace`/`unref_aspace` with their resident-wf precondition)
                    // and `report`/`bind_slots`/`state` (its own structural postconditions) to
                    // have come through the detach unchanged (plan §6d-final-thread-body-2).
                    &&& final(store).tcb_view()[t].cspace == old(store).tcb_view()[t].cspace
                    &&& final(store).tcb_view()[t].aspace == old(store).tcb_view()[t].aspace
                    &&& final(store).tcb_view()[t].state == old(store).tcb_view()[t].state
                    &&& final(store).tcb_view()[t].report == old(store).tcb_view()[t].report
                    &&& final(store).tcb_view()[t].retval == old(store).tcb_view()[t].retval
                    &&& final(store).tcb_view()[t].bind_bits == old(store).tcb_view()[t].bind_bits
                    &&& final(store).tcb_view()[t].bind_slots == old(store).tcb_view()[t].bind_slots
                }
        }),
        // Dead, queue-detached TCBs are frozen across the splice (plan §6d-final-thread-body):
        // a signal-shaped edit — only `t` and its chain predecessor (both `wait_notif == Some(n)`)
        // move, and `refs` drops only at `n` (which had a waiter, so `refs[n] > 0`). So a
        // `wait_notif is None`, `refs == 0` object is untouched. `destroy_tcb` reads it off for
        // its own promise about the *other* dead objects (its subject is excepted separately).
        cspace::dead_tcb_frozen(old(store), final(store)),
{
    let ghost nv0 = old(store).notif_view();
    let ghost tv0 = old(store).tcb_view();
    let ghost ws0 = cspace::waiter_seq(nv0, tv0, n);
    assert(cspace::waiter_chain(nv0, tv0, n, ws0));

    let mut cur = store.notif_wait_head(n);
    let mut prev: Option<ObjId> = None;
    let ghost mut k: int = 0;

    while cur.is_some()
        invariant
            // the walk is read-only: all seven views pinned to `old`.
            store.slot_view() == old(store).slot_view(),
            store.refs_view() == old(store).refs_view(),
            store.chan_view() == old(store).chan_view(),
            store.notif_view() == nv0,
            store.tcb_view() == tv0,
            store.timer_view() == old(store).timer_view(),
            store.timer_head_view() == old(store).timer_head_view(),
            // residency too (plan §6d-final-thread): the walk never touches `cspace_view`, so
            // the absent-path post-state can frame it, and `destroy_tcb` carries it forward.
            store.cspace_view() == old(store).cspace_view(),
            // pin the pre-loop ghosts to the function entry state — a loop body only
            // assumes the invariant, so without this the `nv0 == old(store)...` links
            // (needed for the dom/contract postconditions at the in-loop return) are lost.
            nv0 == old(store).notif_view(),
            tv0 == old(store).tcb_view(),
            ws0 == cspace::waiter_seq(nv0, tv0, n),
            // the chain + the refs side-condition survive into the body.
            cspace::waiter_chain(nv0, tv0, n, ws0),
            nv0.dom().contains(n),
            nv0[n].wait_head is Some
                ==> store.refs_view().dom().contains(n) && store.refs_view()[n] > 0,
            // `cur`/`prev` track position `k` in `ws0`, no `t` seen yet.
            0 <= k <= ws0.len(),
            cur == (if k < ws0.len() { Some(ws0[k]) } else { None::<ObjId> }),
            prev == (if k == 0 { None::<ObjId> } else { Some(ws0[k - 1]) }),
            forall|i: int| 0 <= i < k ==> ws0[i] != t,
        decreases ws0.len() - k,
    {
        let c = cur.unwrap();
        assert(k < ws0.len());
        assert(c == ws0[k]);
        // `ObjId`'s exec `==` is external (cspace.rs:4094); compare the u64 tag. In spec,
        // `c == ws0[k]` ⇒ `c.0 == t.0` reflects `ws0[k] == t`.
        if c.0 == t.0 {
            // `c == ws0[k]` and `c.0 == t.0` ⇒ `t == ws0[k]` (single-field struct eq),
            // the bridge from the tag test to the chain element under `t`.
            assert(t == ws0[k]);
            let ghost len = ws0.len() as int;
            assert(ws0.len() > 0);
            assert(nv0[n].wait_head == Some(ws0[0]));
            assert(nv0[n].wait_tail == Some(ws0[len - 1]));
            // The tail names `t` iff `t` is the last element (no_duplicates).
            assert((ws0[len - 1] == t) == (k == len - 1)) by {
                if ws0[len - 1] == t {
                    assert(ws0[len - 1] == ws0[k]);
                }
                if k == len - 1 {
                    assert(ws0[len - 1] == ws0[k]);
                }
            }

            let next = store.tcb_qnext(c);
            assert(next == tv0[t].qnext);

            // The branchy writes make the views conditional, so dom-preservation is
            // asserted *inside* each arm (where the inserted key is in scope) and
            // path-merges, rather than relying on the merged conditional (which loses
            // the match-bound predecessor key).
            match prev {
                None => {
                    store.set_notif_wait_head(n, next);       // notif insert on resident `n`
                    proof { assert(store.notif_view().dom() =~= nv0.dom()); }
                }
                Some(p) => {
                    proof { assert(k > 0); assert(p == ws0[k - 1]); assert(tv0.dom().contains(p)); }
                    store.set_tcb_qnext(p, next);             // tcb insert on resident `p`
                    proof { assert(store.tcb_view().dom() =~= tv0.dom()); }
                }
            }
            proof {
                assert(store.notif_view().dom() =~= nv0.dom());
                assert(store.tcb_view().dom() =~= tv0.dom());
            }
            // The match left `n`'s tail untouched, so the test below reads `nv0`'s tail.
            assert(store.notif_view()[n].wait_tail == nv0[n].wait_tail);

            // `Option<ObjId>`'s exec `==` is external; match on the tag instead.
            let tail_is_t = match store.notif_wait_tail(n) {
                Some(tl) => tl.0 == t.0,
                None => false,
            };
            if tail_is_t {
                store.set_notif_wait_tail(n, prev);           // notif insert on resident `n`
                proof { assert(store.notif_view().dom() =~= nv0.dom()); }
            }
            store.set_tcb_qnext(t, None);                     // tcb inserts on resident `t`
            store.set_tcb_wait_notif(t, None);
            store.set_obj_refs(n, store.obj_refs(n) - 1);
            proof {
                assert(store.tcb_view().dom() =~= tv0.dom());
                assert(store.notif_view().dom() =~= nv0.dom());
            }

            proof {
                let nvf = store.notif_view();
                let tvf = store.tcb_view();
                // `t == ws0[k]` ⇒ `index_of(t) == k` (no_duplicates), so the splice the
                // contract states (`ws0.remove(index_of(t))`) is `ws0.remove(k)`.
                assert(ws0.contains(t));
                assert(ws0.index_of(t) == k) by {
                    let idx = ws0.index_of(t);
                    assert(0 <= idx < ws0.len() && ws0[idx] == t);
                }
                cspace::lemma_remove_chain(nv0, tv0, nvf, tvf, n, t, ws0, k);
                cspace::lemma_waiter_chain_unique(nvf, tvf, n,
                    cspace::waiter_seq(nvf, tvf, n), ws0.remove(k));
                assert(cspace::notif_wf(nvf, tvf, n));
                // census_delta_frozen: `refs[n]` dropped by one, matched by `waiter_refs(n)`
                // losing `t` (`waiter_seq(n) == ws0.remove(k)`, one shorter); every other
                // object's census is framed. Only `t` and its chain predecessor moved, both
                // chain nodes naming `n`, so for `x != n` no node of `x`'s chain changed;
                // cspace/aspace untouched everywhere. So `refs[x] - census(x)` is frozen.
                ws0.remove_ensures(k);
                assert(cspace::waiter_refs(nv0, tv0, n) == ws0.len());
                assert(cspace::waiter_refs(nvf, tvf, n) == ws0.remove(k).len());
                assert forall|kk: ObjId| #[trigger] tvf[kk] != tv0[kk]
                    implies tv0[kk].wait_notif == Some(n) by {
                    if tvf[kk] != tv0[kk] {
                        assert(tvf[kk].qnext != tv0[kk].qnext || tvf[kk].wait_notif != tv0[kk].wait_notif);
                    }
                }
                assert(store.refs_view().dom() == old(store).refs_view().dom());
                assert forall|x: ObjId| old(store).refs_view().dom().contains(x) implies
                    store.refs_view()[x] + cspace::obj_census(old(store), x)
                        == old(store).refs_view()[x] + #[trigger] cspace::obj_census(store, x) by {
                    cspace::lemma_thread_hold_frame(tv0, tvf, x);
                    if x != n {
                        assert forall|kk: ObjId| #[trigger] tvf[kk] != tv0[kk]
                            implies tv0[kk].wait_notif != Some(x)
                                && tvf[kk].wait_notif != Some(x) by {
                            if tvf[kk] != tv0[kk] {
                                assert(tv0[kk].wait_notif == Some(n));
                            }
                        }
                        cspace::lemma_waiter_refs_frame(nv0, tv0, nvf, tvf, n, x);
                    }
                }
                // refcount_sound (conditional, plan §6f): the frozen delta just established
                // bridges it.
                assert(cspace::census_delta_frozen(old(store), store));
                if cspace::refcount_sound(old(store)) {
                    cspace::lemma_refcount_sound_from_frozen(old(store), store);
                }
                // ── Teardown system invariants survive the splice (plan §6d-final-thread,
                //    the `signal`→`fire` precedent). The splice is signal-shaped: only `n`'s
                //    notif head/tail moved, only `n`'s waiters (`t` + its predecessor) moved,
                //    and every TCB's `bind_slots`/`cspace` is fixed (the setters struct-update). ──
                assert(store.slot_view() == old(store).slot_view());
                assert(store.chan_view() == old(store).chan_view());
                assert(store.cspace_view() == old(store).cspace_view());
                assert(nvf =~= nv0.insert(n, nvf[n]));
                assert forall|kk: ObjId| old(store).tcb_view()[kk].wait_notif != Some(n)
                    implies #[trigger] tvf[kk] == tv0[kk] by {
                    if tvf[kk] != tv0[kk] { assert(tv0[kk].wait_notif == Some(n)); }
                }
                assert forall|kk: ObjId| #[trigger] tvf[kk].bind_slots == tv0[kk].bind_slots by {}
                assert forall|kk: ObjId| #[trigger] tvf[kk].cspace == tv0[kk].cspace by {}
                // `t`'s remaining non-queue fields are untouched (the splice writes only `t`'s
                // `qnext`/`wait_notif`), so the field-frame ensures hold (plan §6d-final-thread-body-2).
                // Single-key asserts (not domain `forall`s) keep the hot loop body under rlimit
                // (the doc-25 §2 decomposition discipline).
                assert(tvf[t].aspace == tv0[t].aspace);
                assert(tvf[t].state == tv0[t].state);
                assert(tvf[t].report == tv0[t].report);
                assert(tvf[t].retval == tv0[t].retval);
                assert(tvf[t].bind_bits == tv0[t].bind_bits);
                // A changed TCB still blocked in the post-state is blocked on `n` (the
                // waiter-coherence frame, plan §6d-final-thread): the only changed TCBs are `t`
                // (its `wait_notif` cleared to `None`, so not `Some(wn)`) and `t`'s predecessor
                // (only its `qnext` moved — still `wait_notif == Some(n)`, so `wn == n`).
                assert forall|kk: ObjId| #[trigger] tvf[kk] != tv0[kk]
                    && tvf[kk].state == ThreadState::BlockedNotif
                    implies (tvf[kk].wait_notif matches Some(wn) ==> wn == n) by {
                    if tvf[kk] != tv0[kk] && kk != t {
                        // Only `t`'s predecessor `p` is otherwise touched, and it kept
                        // `wait_notif == Some(n)` (the splice re-threads its `qnext` only).
                        assert(tv0[kk].wait_notif == Some(n));
                    }
                }
                if cspace::caps_consistent(old(store)) {
                    cspace::lemma_caps_consistent_frame(old(store), store, n);
                }
                if cspace::end_caps_sound(old(store)) {
                    assert(cspace::end_caps_sound(store));
                }
                if cspace::census_dom_complete(old(store)) {
                    // Every census term is framed for `o != n`; `n`'s waiter term dropped one.
                    // So `census(store,o) <= census(old,o)` everywhere, and the refs domain is
                    // unchanged (line above), so the coverage carries.
                    assert forall|o: ObjId| o != n implies #[trigger] cspace::obj_census(store, o)
                        == cspace::obj_census(old(store), o) by {
                        cspace::lemma_thread_hold_frame(tv0, tvf, o);
                        assert forall|kk: ObjId| #[trigger] tvf[kk] != tv0[kk]
                            implies tv0[kk].wait_notif != Some(o) && tvf[kk].wait_notif != Some(o) by {
                            if tvf[kk] != tv0[kk] { assert(tv0[kk].wait_notif == Some(n)); }
                        }
                        cspace::lemma_waiter_refs_frame(nv0, tv0, nvf, tvf, n, o);
                    }
                    assert(cspace::obj_census(store, n) + 1 == cspace::obj_census(old(store), n)) by {
                        cspace::lemma_thread_hold_frame(tv0, tvf, n);
                    }
                    assert forall|o: ObjId| #[trigger] cspace::obj_census(store, o) >= 1
                        implies store.refs_view().dom().contains(o) by {
                        if o != n {
                            assert(cspace::obj_census(old(store), o) == cspace::obj_census(store, o));
                        }
                        cspace::lemma_in_refs_from_census(old(store), o);
                    }
                }
                // dead_tcb_frozen (present): only `n`'s waiters moved, and `refs` dropped only at
                // `n` (which had a waiter, so `refs[n] > 0`) — so a dead, detached object is frozen.
                assert(old(store).refs_view()[n] > 0);
                assert(store.refs_view()
                    == old(store).refs_view().insert(n, (old(store).refs_view()[n] - 1) as nat));
                assert forall|x: ObjId|
                    old(store).refs_view().dom().contains(x) && old(store).refs_view()[x] == 0
                    implies #[trigger] store.refs_view()[x] == 0 by {
                    assert(x != n);
                }
                assert forall|kk: ObjId| #[trigger] store.tcb_view()[kk] == old(store).tcb_view()[kk]
                    || old(store).tcb_view()[kk].wait_notif == Some(n) by {}
                assert(store.refs_view().dom() =~= old(store).refs_view().dom());
                assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
                cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
            }
            return;
        }
        prev = cur;
        cur = store.tcb_qnext(c);
        proof {
            k = k + 1;
        }
    }
    // Fell off the end ⇒ k == ws0.len() ⇒ `t` was never on the queue; store == old.
    proof {
        assert(k == ws0.len());
        assert(!ws0.contains(t));
        assert(cspace::notif_wf(store.notif_view(), store.tcb_view(), n));
        // The walk is read-only (the loop invariant pins every view to `old`), so refs and
        // census are untouched — the delta is trivially frozen.
        assert(store.notif_view() == old(store).notif_view());
        assert(store.tcb_view() == old(store).tcb_view());
        assert(store.refs_view() == old(store).refs_view());
        assert(store.cspace_view() == old(store).cspace_view());
        assert forall|o: ObjId| #[trigger] cspace::obj_census(store, o)
            == cspace::obj_census(old(store), o) by {}
        // refcount_sound (conditional, plan §6f): the store is unchanged, so it carries.
        assert(cspace::census_delta_frozen(old(store), store));
        if cspace::refcount_sound(old(store)) {
            cspace::lemma_refcount_sound_from_frozen(old(store), store);
        }
        // The store is unchanged, so the teardown system invariants carry trivially (plan
        // §6d-final-thread).
        assert(cspace::caps_consistent(old(store)) ==> cspace::caps_consistent(store));
        assert(cspace::end_caps_sound(old(store)) ==> cspace::end_caps_sound(store));
        assert(cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(store));
        // dead_tcb_frozen (absent): the store is unchanged, so it is trivially frozen.
        assert forall|kk: ObjId| #[trigger] store.tcb_view()[kk] == old(store).tcb_view()[kk]
            || old(store).tcb_view()[kk].wait_notif == Some(n) by {}
        assert(store.refs_view().dom() =~= old(store).refs_view().dom());
        assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
        cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
    }
}

} // verus!
