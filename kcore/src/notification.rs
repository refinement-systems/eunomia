//! Notification objects (spec rev2§3.6): a machine word of signal bits plus a
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
// `store.notif_view()`/`tcb_view()`/… in the contracts; it erases in a
// normal build, so it is otherwise unused here.
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
/// Proven body against the `waiter_seq` FIFO
/// model. The `slot_view`/`chan_view`-unchanged frame lets the
/// callers `fire`/`send`/`recv`/`endpoint_cap_dropped` use `signal`'s result
/// unchanged; the
/// preconditions (the notification is live + `notif_wf`, and a queued waiter implies
/// `refs > 0`) are discharged by the channel ops via `cspace::binding_notif_wf`. On
/// the **wake** path the head waiter is dequeued (`waiter_seq` loses its head —
/// `Seq::drop_first`), receives the whole accumulated word, the word clears, the
/// queued ref is released (`refs -= 1`), and the thread is made Runnable; on the
/// **accumulate** path the word grows and the queue/refs are untouched. `notif_wf`
/// is preserved either way.
// `spinoff_prover`: the wake path carries the `make_runnable` enqueue + ready-queue/`p_opt`
// term families inline (extracting the census via `cspace::lemma_waiter_dequeue_census`
// costs more than it saves here — §10 dead-end documented at :379-383), pushing this body
// past the shared module batch budget. No `rlimit` cap: verifies at default (~21M rlimit).
#[verifier::spinoff_prover]
pub fn signal<S: Store>(store: &mut S, n: ObjId, bits: u64)
    requires
        old(store).notif_view().dom().contains(n),
        cspace::notif_wf(old(store).notif_view(), old(store).tcb_view(), n),
        old(store).notif_view()[n].wait_head is Some ==> old(store).refs_view().dom().contains(n)
            && old(store).refs_view()[n] > 0,
        // The wake faithfully enqueues the woken thread, so `signal` carries the
        // ready-queue invariants. The cascade callers supply them (via `lemma_ready_inv_frame`
        // across object-only steps); `make_runnable` re-establishes them across the enqueue.
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        // `signal` touches no cspace residency (every setter frames it) — the frame
        // `lemma_caps_consistent_frame` (via `fire`) needs.
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        // The refcount census moves in lockstep: a wake drops `refs[n]`
        // and `waiter_seq(n)` together, so `refs[x] - census(x)` is frozen at every `x`.
        // Unconditional and `requires`-free, so the kernel-shell callers
        // `report_terminal`/`check_expired` and the construction-op callers of `fire`
        // (`send`/`recv`) are undisturbed; `delete` consumes it across the off-by-one window.
        cspace::census_delta_frozen(old(store), final(store)),
        // `refcount_sound` as a *system* invariant: a sound census in, a sound
        // census out. Conditional, so census-agnostic callers (`check_expired`'s
        // `signal`-in-a-loop, `report_terminal`) stay undisturbed; it is the frozen delta
        // (above) bridged by `lemma_refcount_sound_from_frozen`.
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
        // A census off by one at any `z` survives the wake (it is the frozen delta applied to
        // that shape) — `delete`'s Channel branch reads this off the fire chain to carry the
        // deleted-slot off-by-one across the peer-closed fire. The trigger keeps it out of
        // census-agnostic callers (`check_expired`'s `signal`-in-a-loop).
        forall|z: ObjId|
            cspace::census_off_by_one(old(store), z) ==> #[trigger] cspace::census_off_by_one(
                final(store),
                z,
            ),
        // Refs-domain completeness survives the wake (the census only drops, the refs domain
        // is unchanged) — `delete`'s Channel branch carries it across the fire to `obj_unref`.
        // Conditional + obj_census-triggered, so `check_expired` is undisturbed.
        cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(final(store)),
        // Dead, queue-detached TCBs are frozen across the wake: a
        // signal touches only the woken head (`wait_notif == Some(n)`) and drops `refs[n]` (which
        // a waiter held, so `refs[n] > 0`), so a `wait_notif is None`, `refs == 0` object `x` is
        // not the head (`x != n`) and is untouched. `fire`/`endpoint_cap_dropped`/`delete` carry
        // it up the teardown chain.
        cspace::dead_tcb_frozen(old(store), final(store)),
        // "Dead stays dead" across the wake: the accumulate path frames `refs` whole; the
        // wake path drops only `refs[n]` (which a waiter held, so `refs[n] > 0`), keeping the
        // domain — so a dead object stays dead. `fire`/`endpoint_cap_dropped`/`delete` carry it.
        cspace::refs_death_persist(old(store), final(store)),
        // The timer views are untouched: every setter in the body frames
        // them and `make_runnable` frames them, so `report_terminal` (which fires
        // `signal` and otherwise touches no timer) can frame timers across the wake.
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        // The whole effect is confined to notification `n` (in `notif_view`) and the
        // one woken thread (in `tcb_view`); the domains never move.
        final(store).notif_view() == old(store).notif_view().insert(
            n,
            final(store).notif_view()[n],
        ),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        // `signal` touches only TCBs that were waiting on `n` (the woken head) or the old
        // ready-tail it re-threads on enqueue (a Runnable thread) — the frame a caller needs to
        // keep *other* notifications' `notif_wf` intact across a fire (`lemma_notif_wf_frame`).
        // Stated in **contrapositive** form ("a *changed* TCB was an `n`-waiter or was
        // Runnable"): the faithful enqueue perturbs the old level-tail `p` (Runnable,
        // `wait_notif None`), which a `wait_notif`-only frame would wrongly claim unchanged.
        // The contrapositive is vacuous on out-of-domain phantom keys (which `signal` never
        // perturbs), so a caller can frame any `m != n`'s waiters — each `BlockedNotif`
        // (non-Runnable by `ready_complete`) with `wait_notif == Some(m) != Some(n)`, hence
        // *not* in the changed set — across the wake, without reasoning about phantom keys.
        forall|k: ObjId| #[trigger]
            final(store).tcb_view()[k] != old(store).tcb_view()[k] ==> old(
                store,
            ).tcb_view()[k].wait_notif == Some(n) || old(store).tcb_view()[k].state
                == ThreadState::Runnable,
        cspace::notif_wf(final(store).notif_view(), final(store).tcb_view(), n),
        // `signal` writes only the wake/scheduler fields — the fixups clear `t`'s queue/wait
        // links and set its `retval`, and `make_runnable` sets `state`/`qnext` (on `t` plus the
        // re-threaded old ready-tail). Every *other* field of every thread (`report`, `cspace`,
        // `aspace`, `bind_slots`, `bind_bits`, `priority`) is preserved, so a caller can read its
        // own subject's untouched fields off this (`report_terminal`'s `report`, `bind`'s slots).
        forall|k: ObjId| #[trigger]
            final(store).tcb_view()[k] == (cspace::TcbView {
                state: final(store).tcb_view()[k].state,
                qnext: final(store).tcb_view()[k].qnext,
                retval: final(store).tcb_view()[k].retval,
                wait_notif: final(store).tcb_view()[k].wait_notif,
                ..old(store).tcb_view()[k]
            }),
        // The wake produces only Runnable threads — it sets the woken
        // `BlockedNotif` thread to Runnable and re-threads the old ready-tail's `qnext` (which
        // stays Runnable). So **a thread that ends `BlockedNotif` was unchanged** by the wake;
        // `fire`/`delete`'s caps + waiter-coherence proofs use this to frame every still-blocked
        // waiter (the only changed nodes are non-blocked, so they cannot be stray waiters).
        forall|k: ObjId| #[trigger]
            final(store).tcb_view()[k].state == ThreadState::BlockedNotif
                ==> final(store).tcb_view()[k] == old(store).tcb_view()[k],
        // The ready-queue invariants survive the wake (enqueue re-establishes them; the
        // accumulate path frames the views).
        cspace::ready_wf(final(store).ready_view(), final(store).tcb_view()),
        cspace::ready_complete(final(store).ready_view(), final(store).tcb_view()),
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
            &&& old(store).tcb_view()[t].wait_notif == Some(
                n,
            )
            // The faithful enqueue changes only `t` and the old ready-tail of `t`'s level
            // (its `qnext` retargeted to `t`); every other TCB is framed. The wake perturbs
            // two nodes: `t` and that ready-tail.
            &&& forall|k: ObjId|
                #![trigger final(store).tcb_view()[k]]
                k != t && Some(k) != old(store).ready_view().tails[old(
                    store,
                ).tcb_view()[t].priority as int] ==> final(store).tcb_view()[k] == old(
                    store,
                ).tcb_view()[k]
            &&& final(store).tcb_view()[t].state == ThreadState::Runnable
            &&& final(store).tcb_view()[t].retval == (old(store).notif_view()[n].word
                | bits)
            // The woken thread's binding slots are untouched — `signal` moves only its
            // queue/wait/retval fields. The frame `caps_consistent` preservation needs (a
            // Thread cap for `t` reads its `bind_slots`).
            &&& final(store).tcb_view()[t].bind_slots == old(
                store,
            ).tcb_view()[t].bind_slots
            // …and its bound cspace/aspace are untouched too — the strengthened
            // `cap_consistent(Thread)` clause reads `tcb[t].cspace` (its `cspace_resident_wf`),
            // so `lemma_caps_consistent_frame` needs the cspace frame across the wake; the
            // aspace half rides the same proof (`thread_hold_refs`).
            &&& final(store).tcb_view()[t].cspace == old(store).tcb_view()[t].cspace
            &&& final(store).tcb_view()[t].aspace == old(store).tcb_view()[t].aspace
            &&& final(store).notif_view()[n].word == 0
            &&& final(store).refs_view() == old(store).refs_view().insert(
                n,
                (old(store).refs_view()[n] - 1) as nat,
            )
            &&& cspace::waiter_seq(final(store).notif_view(), final(store).tcb_view(), n)
                == cspace::waiter_seq(
                old(store).notif_view(),
                old(store).tcb_view(),
                n,
            ).drop_first()
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
            assert forall|k: ObjId|
                old(store).tcb_view()[k].wait_notif != Some(
                    n,
                ) implies #[trigger] store.tcb_view()[k] == old(store).tcb_view()[k] by {}
            assert(cspace::waiter_chain(store.notif_view(), store.tcb_view(), n, ws0));
            cspace::lemma_waiter_chain_unique(
                store.notif_view(),
                store.tcb_view(),
                n,
                cspace::waiter_seq(store.notif_view(), store.tcb_view(), n),
                ws0,
            );
            // census_delta_frozen: refs untouched; the census is framed — only `nv[n].word`
            // moved (not a census term), `tv` is identical, and the slot/chan/timer views are
            // framed, so `waiter_refs(o)` rides through for every `o` (it reads `nv` only at
            // `o`, framed for `o != n`; the chain at `n` is `ws0` either way). With refs and
            // census both unchanged, the delta is trivially frozen.
            assert forall|o: ObjId| #[trigger]
                cspace::obj_census(store, o) == cspace::obj_census(old(store), o) by {
                if o != n {
                    cspace::lemma_waiter_refs_frame_nv(
                        old(store).notif_view(),
                        store.notif_view(),
                        store.tcb_view(),
                        o,
                    );
                }
            }
            assert(cspace::census_delta_frozen(old(store), store));
            // refcount_sound (conditional): the frozen delta bridges it.
            if cspace::refcount_sound(old(store)) {
                cspace::lemma_refcount_sound_from_frozen(old(store), store);
            }
            assert forall|z: ObjId|
                cspace::census_off_by_one(
                    old(store),
                    z,
                ) implies #[trigger] cspace::census_off_by_one(store, z) by {
                cspace::lemma_off_by_one_frozen(old(store), store, z);
            }
            // census_dom_complete: census + refs domain both unchanged ⇒ coverage carries.
            if cspace::census_dom_complete(old(store)) {
                assert forall|o: ObjId| #[trigger]
                    cspace::obj_census(store, o) >= 1 implies store.refs_view().dom().contains(
                    o,
                ) by {}
            }
            // dead_tcb_frozen: the accumulate path moved no TCB and no ref, so every dead,
            // detached object is trivially frozen (signal-shaped with no waiter moved).

            assert(store.refs_view() == old(store).refs_view());
            assert forall|k: ObjId| #[trigger]
                store.tcb_view()[k] == old(store).tcb_view()[k] || old(
                    store,
                ).tcb_view()[k].wait_notif == Some(n) by {}
            assert(store.refs_view().dom() =~= old(store).refs_view().dom());
            assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
            cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
            // "Dead stays dead": the accumulate path frames `refs` whole, so death is preserved.
            cspace::lemma_refs_death_persist_from_refs_eq(old(store), store);
            // The accumulate path frames `ready_view`+`tcb_view`, so the ready invariants carry.
            cspace::lemma_ready_inv_frame(old(store), store);
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
    proof {
        // `make_runnable`'s preconditions. `t` is still `BlockedNotif` (the fixups touched only
        // its queue/wait/retval fields), in priority range (the strengthened `waiter_chain`
        // covenant), and now non-waiting (`wait_notif` cleared at :251). The ready invariants
        // ride the fixups — a non-Runnable edit (only `t`, still blocked) — via the offchain frame.
        assert(cspace::waiter_chain(nv0, tv0, n, ws0));
        assert((tv0[t].priority as int) < crate::sysabi::NUM_PRIOS) by {
            assert(ws0[0] == t);
        }
        assert(tv0[t].state == ThreadState::BlockedNotif);
        assert(store.tcb_view()[t].priority == tv0[t].priority);
        assert(store.tcb_view()[t].state == tv0[t].state);
        assert(store.tcb_view()[t].wait_notif is None);
        assert forall|x: ObjId| #[trigger]
            store.tcb_view()[x] != old(store).tcb_view()[x] implies old(store).tcb_view()[x].state
            != ThreadState::Runnable && store.tcb_view()[x].state != ThreadState::Runnable by {
            assert(x == t);
            assert(tv0[t].state == ThreadState::BlockedNotif);
        }
        assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
        assert(store.ready_view() == old(store).ready_view());
        cspace::lemma_ready_inv_frame_offchain(old(store), store);
    }
    store.make_runnable(t);

    proof {
        let dws = ws0.drop_first();
        let nvf = store.notif_view();
        let tvf = store.tcb_view();
        let ghost level = tv0[t].priority as int;
        let ghost p_opt = old(store).ready_view().tails[level];
        // The changed set (old → final) is exactly `{t} ∪ {p}` where `p` is the old ready-tail
        // of `t`'s level (its `qnext` retargeted to `t` by the enqueue). `p` (if any) is Runnable
        // in `tv0`, hence `wait_notif None`; the enqueue preserves its home fields (global frame).
        assert(p_opt matches Some(p) ==> {
            &&& tv0[p].state == ThreadState::Runnable
            &&& tv0[p].wait_notif is None
            &&& p != t
        }) by {
            if let Some(p) = p_opt {
                let rs0 = cspace::ready_seq(old(store).ready_view(), tv0, level);
                assert(cspace::ready_chain(old(store).ready_view(), tv0, level, rs0));
                assert(rs0.len() > 0 && rs0[rs0.len() - 1] == p);
            }
        }
        assert forall|k: ObjId| #[trigger] tvf[k] != tv0[k] implies k == t || Some(k) == p_opt by {
            if k != t && Some(k) != p_opt {
                assert(tvf[k] == tv0[k]);
            }
        }
        // The imperative fixups touched only `t`; the enqueue additionally re-threaded the old
        // ready-tail `p`. The waiters of `n` (the chain `dws`) are neither, so they are frozen.
        assert(tv0[t].wait_notif == Some(n));
        assert(tv0.dom().contains(ws0[0]));
        assert(tvf.dom() == tv0.dom());
        assert forall|k: ObjId|
            #![trigger tvf[k]]
            k != t && tv0[k].wait_notif == Some(n) implies tvf[k] == tv0[k] by {
            // a waiter of `n` is not the Runnable old ready-tail `p`.
            if Some(k) == p_opt {
            }
        }
        // The contract's weakened tcb frame: non-`n`-waiters that are *also* non-Runnable are
        // frozen (the wake perturbs only the woken head `t` and the Runnable old ready-tail `p`).
        assert forall|k: ObjId|
            old(store).tcb_view()[k].wait_notif != Some(n) && old(store).tcb_view()[k].state
                != ThreadState::Runnable implies #[trigger] tvf[k] == old(store).tcb_view()[k] by {}
        cspace::lemma_drop_first_chain(nv0, tv0, nvf, tvf, n, t, ws0);
        cspace::lemma_waiter_chain_unique(nvf, tvf, n, cspace::waiter_seq(nvf, tvf, n), dws);
        // refcount_sound (conditional): `refs[n]` dropped by one, matched by `waiter_refs(n)`
        // losing the woken head (`waiter_seq(n) == ws0.drop_first()`); every other object's
        // census is framed. The slot/chan/timer terms ride the view frames; `thread_hold`
        // rides `lemma_thread_hold_frame` (only `t`'s queue/wait fields moved, never cspace/
        // aspace); `waiter_refs(o)` for `o != n` rides `lemma_waiter_refs_frame`.
        assert(tvf[t].cspace == tv0[t].cspace);
        assert(tvf[t].aspace == tv0[t].aspace);
        // cspace/aspace are framed for *every* thread: the fixups + the enqueue write only
        // `state`/`qnext`/`wait_notif`/`retval`, never the bound cspace/aspace (the enqueue's
        // global frame preserves them for `t` and the old ready-tail `p` alike).
        assert forall|k: ObjId| #[trigger] tvf[k].cspace == tv0[k].cspace by {}
        assert forall|k: ObjId| #[trigger] tvf[k].aspace == tv0[k].aspace by {}
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
        //
        // Proven inline rather than via `cspace::lemma_waiter_dequeue_census` (the shared map
        // lemma `remove_waiter` uses): the wake's `make_runnable` enqueue leaves this context
        // carrying the ready-queue/`p_opt` term families, so discharging that lemma's `requires`
        // here costs more than the inline derivation saves (§10's "small context" payoff only
        // lands when the caller's context is already small).
        assert forall|o: ObjId| #[trigger]
            cspace::obj_census(store, o) == (if o == n {
                (cspace::obj_census(old(store), n) - 1) as nat
            } else {
                cspace::obj_census(old(store), o)
            }) by {
            cspace::lemma_thread_hold_frame(tv0, tvf, o);
            if o != n {
                assert forall|k: ObjId| #[trigger] tvf[k] != tv0[k] implies tv0[k].wait_notif
                    != Some(o) && tvf[k].wait_notif != Some(o) by {
                    if tvf[k] != tv0[k] {
                        // changed ⇒ `k` is the woken head `t` (`wait_notif Some(n)→None`) or the
                        // old ready-tail `p` (Runnable ⇒ `wait_notif None`, preserved by the
                        // enqueue). Both have `wait_notif != Some(o)` before and after, for `o != n`.
                        if k == t {
                            assert(tv0[t].wait_notif == Some(n));
                            assert(tvf[t].wait_notif is None);
                        } else {
                            assert(Some(k) == p_opt);
                            assert(tv0[k].wait_notif is None);
                            assert(tvf[k].wait_notif == tv0[k].wait_notif);
                        }
                    }
                }
                cspace::lemma_waiter_refs_frame(nv0, tv0, nvf, tvf, n, o);
            }
        }
        assert(cspace::census_delta_frozen(old(store), store));
        // refcount_sound (conditional): the frozen delta bridges it.
        if cspace::refcount_sound(old(store)) {
            cspace::lemma_refcount_sound_from_frozen(old(store), store);
        }
        assert forall|z: ObjId|
            cspace::census_off_by_one(old(store), z) implies #[trigger] cspace::census_off_by_one(
            store,
            z,
        ) by {
            cspace::lemma_off_by_one_frozen(old(store), store, z);
        }
        if cspace::census_dom_complete(old(store)) {
            assert forall|o: ObjId| #[trigger]
                cspace::obj_census(store, o) >= 1 implies store.refs_view().dom().contains(o) by {
                // census(store, o) <= census(old, o), so a positive-census o had one before ⇒
                // it was covered; the domain is unchanged.
                assert(cspace::obj_census(old(store), o) >= 1);
            }
        }
        // dead_tcb_frozen: a signal-shaped edit. The woken head `t` (`wait_notif == Some(n)`) and
        // the old ready-tail `p` (Runnable) moved; `refs` dropped only at `n`, which had
        // `refs > 0` (the woken waiter held it). A dead (`refs 0`), detached (`wait_notif None`),
        // *non-Runnable* `x` is none of `{t, p, n}`, so it is frozen (`dead_tcb_frozen_at`).

        assert(old(store).refs_view()[n] > 0);
        assert(store.refs_view() == old(store).refs_view().insert(
            n,
            (old(store).refs_view()[n] - 1) as nat,
        ));
        assert forall|x: ObjId|
            old(store).refs_view().dom().contains(x) && old(store).refs_view()[x]
                == 0 implies #[trigger] store.refs_view()[x] == 0 by {
            assert(x != n);
        }
        assert forall|k: ObjId| #[trigger]
            store.tcb_view()[k] == old(store).tcb_view()[k] || old(store).tcb_view()[k].wait_notif
                == Some(n) || old(store).tcb_view()[k].state == ThreadState::Runnable by {}
        assert(store.refs_view().dom() =~= old(store).refs_view().dom());
        assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
        cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
        // "Dead stays dead": the wake drops only `refs[n]` (positive), keeping the domain — death preserved.
        cspace::lemma_refs_death_persist_dec_ref(old(store), store, n);
    }
}

/// Wait: consume the word if nonzero, else block the current thread.
///
/// On a nonzero word the thread returns it and the word clears
/// (the queue/refs are untouched); otherwise `cur` is appended at the tail
/// (`waiter_seq` grows by `Seq::push` — the FIFO/block-order half of the wake-order
/// theorem), is marked `BlockedNotif`/`wait_notif = Some(n)`, and acquires one ref on
/// `n` (released on wake or teardown). `notif_wf` is preserved. The `cur` thread (the
/// running caller) must not be queued on any notification (`wait_notif is None`): this
/// keeps `n`'s push duplicate-free *and* lets the census frame conclude `cur` is off every
/// other notification's chain, so `waiter_refs(o)` is unperturbed for `o != n`.
///
/// **Refcount census.** Exports `census_delta_frozen` + conditional `refcount_sound`,
/// matching `signal`/`remove_waiter`: on the block path the `refs[n] += 1` acquire is matched
/// by `waiter_refs(n)` gaining `cur` (`waiter_seq` push), the reverse of `signal`'s wake; the
/// consume path moves no census term. This closes the soundness chain so a verified caller
/// after a syscall `wait` can discharge its `refcount_sound(old)` precondition.
pub fn wait<S: Store>(store: &mut S, n: ObjId, cur: ObjId) -> (res: Option<u64>)
    requires
        old(store).notif_view().dom().contains(n),
        old(store).tcb_view().dom().contains(cur),
        cspace::notif_wf(old(store).notif_view(), old(store).tcb_view(), n),
        old(store).tcb_view()[cur].wait_notif is None,
        // The blocking thread's priority is a valid ready-queue level. `wait` is the sole
        // appender to a waiter chain, so this leaf precondition (the kernel supplies it for the
        // running thread) re-establishes the strengthened `waiter_chain` priority covenant; it
        // then rides `notif_wf` to `signal`, which needs it for the faithful `make_runnable`.
        (old(store).tcb_view()[cur].priority as int) < crate::sysabi::NUM_PRIOS,
        old(store).notif_view()[n].word == 0 ==> old(store).refs_view().dom().contains(n) && old(
            store,
        ).refs_view()[n] < u32::MAX,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view().dom() == old(store).notif_view().dom(),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        cspace::notif_wf(final(store).notif_view(), final(store).tcb_view(), n),
        // The refcount census moves in lockstep: block acquires `refs[n]` and a waiter
        // slot together; consume touches neither.
        cspace::census_delta_frozen(old(store), final(store)),
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
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
            &&& final(store).refs_view() == old(store).refs_view().insert(
                n,
                (old(store).refs_view()[n] + 1) as nat,
            )
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
                store.notif_view(),
                store.tcb_view(),
                n,
                cspace::waiter_seq(store.notif_view(), store.tcb_view(), n),
                ws0,
            );
            // Census: only `nv[n].word` moved (not a census term), `refs`/`tcb` untouched
            // and the slot/chan/timer views framed, so `waiter_refs(o)` rides through for every
            // `o` (`lemma_waiter_refs_frame_nv`). Refs and census both unchanged ⇒ frozen.
            assert(store.refs_view() == old(store).refs_view());
            assert forall|o: ObjId| #[trigger]
                cspace::obj_census(store, o) == cspace::obj_census(old(store), o) by {
                if o != n {
                    cspace::lemma_waiter_refs_frame_nv(
                        old(store).notif_view(),
                        store.notif_view(),
                        store.tcb_view(),
                        o,
                    );
                }
            }
            assert(cspace::census_delta_frozen(old(store), store));
            if cspace::refcount_sound(old(store)) {
                cspace::lemma_refcount_sound_from_frozen(old(store), store);
            }
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
            assert forall|i: int| 0 <= i < pws.len() implies #[trigger] tvf.dom().contains(
                pws[i],
            ) by {
                if i < ws0.len() {
                    assert(pws[i] == ws0[i]);
                } else {
                    assert(pws[i] == cur);
                }
            }
            assert forall|i: int| 0 <= i < pws.len() implies tvf[pws[i]].qnext == (if i + 1
                < pws.len() {
                Some(pws[i + 1])
            } else {
                None
            }) by {
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
            assert forall|i: int| 0 <= i < pws.len() implies tvf[pws[i]].wait_notif == Some(n)
                && tvf[pws[i]].state == ThreadState::BlockedNotif by {
                if i < ws0.len() {
                    assert(pws[i] == ws0[i] && ws0[i] != cur);
                } else {
                    assert(pws[i] == cur);
                }
            }
            // The strengthened priority covenant. `wait` never writes any `priority`
            // (only state/wait_notif/qnext), so the bound rides framed for the `ws0` prefix
            // (from the input chain) and from the new leaf precondition for `cur`.
            assert forall|i: int| 0 <= i < pws.len() implies (tvf[pws[i]].priority as int)
                < crate::sysabi::NUM_PRIOS by {
                if i < ws0.len() {
                    assert(pws[i] == ws0[i]);
                    assert((tv0[ws0[i]].priority as int) < crate::sysabi::NUM_PRIOS);
                    assert(tvf[ws0[i]].priority == tv0[ws0[i]].priority);
                } else {
                    assert(pws[i] == cur);
                    assert(tvf[cur].priority == tv0[cur].priority);
                }
            }
        }
        cspace::lemma_waiter_chain_unique(nvf, tvf, n, cspace::waiter_seq(nvf, tvf, n), pws);

        // Census. The block path acquires `refs[n] += 1`, matched by `waiter_refs(n)`
        // gaining `cur` (`waiter_seq(n) == ws0.push(cur)`, one longer) — the reverse of
        // `signal`'s wake. Every other census term is framed: `thread_hold` via
        // `lemma_thread_hold_frame` (only `cur`/the tail's queue links moved, never
        // cspace/aspace), and `waiter_refs(o)` for `o != n` via `lemma_waiter_refs_frame`.
        let ghost old_tail = nv0[n].wait_tail;
        assert(store.refs_view() == old(store).refs_view().insert(
            n,
            (old(store).refs_view()[n] + 1) as nat,
        ));
        assert(store.refs_view().dom() == old(store).refs_view().dom());
        assert(cspace::waiter_seq(nvf, tvf, n) == pws);
        assert(cspace::waiter_refs(nv0, tv0, n) == ws0.len());
        assert(cspace::waiter_refs(nvf, tvf, n) == pws.len());
        assert(pws.len() == ws0.len() + 1);
        // `wait_notif` is written for `cur` alone (None → Some(n); the tail set touches only
        // `qnext`); the only non-`cur` TCB written is the old tail, which keeps its on-`n`-chain
        // `wait_notif == Some(n)`. So every changed TCB names `n` (or was detached) in both
        // states — the shared shape `lemma_waiter_enqueue_census` keys on.
        assert forall|k: ObjId| #[trigger] tvf[k] != tv0[k] && k != cur implies old_tail == Some(
            k,
        ) by {}
        assert(old_tail matches Some(tl) ==> tv0[tl].wait_notif == Some(n)) by {
            if let Some(tl) = old_tail {
                // The wait_tail is `ws0`'s last node, and every chain node waits on `n`.
                assert(ws0.len() >= 1);
                assert(tl == ws0[ws0.len() - 1]);
                assert(tv0[ws0[ws0.len() - 1]].wait_notif == Some(n));
            }
        }
        assert(tvf.dom() == tv0.dom());
        assert(nvf == nv0.insert(n, nvf[n]));
        assert forall|k: ObjId| #[trigger] tvf[k].cspace == tv0[k].cspace by {}
        assert forall|k: ObjId| #[trigger] tvf[k].aspace == tv0[k].aspace by {}
        assert forall|k: ObjId| #[trigger] tvf[k] != tv0[k] implies (tv0[k].wait_notif is None
            || tv0[k].wait_notif == Some(n)) && (tvf[k].wait_notif is None || tvf[k].wait_notif
            == Some(n)) by {
            if k == cur {
                assert(tv0[cur].wait_notif is None);
            } else {
                assert(old_tail == Some(k));
                assert(tvf[k].wait_notif == tv0[k].wait_notif);
            }
        }
        cspace::lemma_waiter_enqueue_census(old(store), store, n);
        assert(cspace::census_delta_frozen(old(store), store));
        if cspace::refcount_sound(old(store)) {
            cspace::lemma_refcount_sound_from_frozen(old(store), store);
        }
    }
    None
}

/// pre:  refs == 0 — no caps, no bindings, no armed timers, no waiters.
///
/// A no-op. The no-waiters condition is supplied directly
/// (`wait_head is None`) — exactly what the production `debug_assert` checks. The
/// "`refs == 0` ⇒ no waiters" justification (a waiter holds a ref) lives in the
/// refcount census rather than the structural `notif_wf` here, so requiring the
/// empty queue is the honest scoped contract.
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
        final(store).irq_view() == old(store).irq_view(),
        // The model no-op frames the ready queue too — `obj_unref`'s Notification arm
        // frames the ready pair across the destructor.
        final(store).ready_view() == old(store).ready_view(),
        // A model no-op (the kernel reclaims the object's memory; the abstract views are
        // untouched), so the cap→object invariant rides through trivially.
        cspace::caps_consistent(final(store)),
        cspace::end_caps_sound(final(store)),
        cspace::census_dom_complete(final(store)),
{
    let _ = n;
    let _ = store;
}

/// Unlink waiter `t` from notification `n`'s queue (the thread-teardown path).
///
/// The mid-queue unlink — the `cdt_unlink` analog
/// but singly-linked with no re-parenting, so the removal is a plain `Seq`
/// splice. If `t` is queued on `n` it is spliced out (`waiter_seq(n)` loses exactly the
/// `t` element, the FIFO order of the rest preserved — `Seq::remove`), its `qnext`/
/// `wait_notif` are cleared, and the queued ref is released (`refs[n] -= 1`, the
/// waiter term of `refcount_sound`'s census alongside `signal`'s pop-release); if `t`
/// is absent the store is unchanged. `notif_wf(n)` is preserved either way. The walk is
/// read-only — the only writes are on the found path, which returns. The `refs > 0`
/// precondition (a non-empty queue ⇒ live) discharges the release `-1`, exactly as in
/// `signal`.
// With the per-object census map extracted to `cspace::lemma_waiter_dequeue_census`
// (called at :964), this body proves only the cheap local facts (the `-1` waiter delta +
// the changed-TCB shape). The residual splice-walk loop body verifies within the default
// budget without a dedicated Z3 instance; no `spinoff_prover` or `rlimit` cap needed.
pub fn remove_waiter<S: Store>(store: &mut S, n: ObjId, t: ObjId)
    requires
        old(store).notif_view().dom().contains(n),
        cspace::notif_wf(old(store).notif_view(), old(store).tcb_view(), n),
        old(store).notif_view()[n].wait_head is Some ==> old(store).refs_view().dom().contains(n)
            && old(store).refs_view()[n] > 0,
        // The splice moves only `BlockedNotif` nodes (`t` + its chain predecessor), so the
        // ready-queue invariants ride it (the off-chain frame); `destroy_tcb` carries the pair.
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        cspace::ready_wf(final(store).ready_view(), final(store).tcb_view()),
        cspace::ready_complete(final(store).ready_view(), final(store).tcb_view()),
        final(store).notif_view().dom() == old(store).notif_view().dom(),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        // Every TCB's immutable `bind_slots` survive the splice: a signal-shaped edit
        // writes only queue/wait links, never `bind_slots`. `destroy_tcb` reads it off for the
        // `home_views_frozen` stability across its BlockedNotif detach.
        forall|k: ObjId| #[trigger]
            final(store).tcb_view()[k].bind_slots == old(store).tcb_view()[k].bind_slots,
        cspace::notif_wf(final(store).notif_view(), final(store).tcb_view(), n),
        // The refcount census moves in lockstep: the splice
        // drops `refs[n]` and `waiter_seq(n)` (losing `t`) together; absent, nothing moves.
        // Unconditional — `destroy_tcb` turns it into `refcount_sound` via
        // `lemma_refcount_sound_from_frozen` (it calls `remove_waiter` where the census is sound).
        cspace::census_delta_frozen(old(store), final(store)),
        // `refcount_sound` as a system invariant: the frozen delta bridges a sound
        // census in to a sound census out. Conditional + `requires`-free, so the teardown callers
        // keep no obligation; `destroy_tcb` already consumes the frozen delta directly.
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
        // Residency is untouched (the splice writes notif head/tail + tcb queue links + `refs`,
        // never `cspace_view`); `destroy_tcb` carries it to its own `cspace_view` ensures.
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        // The teardown system invariants survive the splice (the `signal`→`fire` precedent):
        // it is a signal-shaped edit (only `n`'s notif view + `n`'s waiter TCBs move, every
        // TCB's `bind_slots`/`cspace` fixed), so `lemma_caps_consistent_frame` applies; the
        // rev2§3.3 endpoint census reads only the framed chan/slot views; and the census only
        // drops while the refs domain is fixed. Conditional + `requires`-free, so the teardown
        // callers keep no obligation; `destroy_tcb` (the only kcore caller) consumes them for
        // its bind-slot `delete`s.
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
                &&& final(store).refs_view() == old(store).refs_view().insert(
                    n,
                    (old(store).refs_view()[n] - 1) as nat,
                )
                // The splice writes only `t`'s queue links (`qnext`/`wait_notif`); `t`'s
                // every *other* field survives. `destroy_tcb` (the only kcore caller) reads
                // this off across the BlockedNotif detach: it needs `t`'s `cspace`/`aspace`
                // (to drive `unref_cspace`/`unref_aspace` with their resident-wf precondition)
                // and `report`/`bind_slots`/`state` (its own structural postconditions) to
                // have come through the detach unchanged.
                &&& final(store).tcb_view()[t].cspace == old(store).tcb_view()[t].cspace
                &&& final(store).tcb_view()[t].aspace == old(store).tcb_view()[t].aspace
                &&& final(store).tcb_view()[t].state == old(store).tcb_view()[t].state
                &&& final(store).tcb_view()[t].report == old(store).tcb_view()[t].report
                &&& final(store).tcb_view()[t].retval == old(store).tcb_view()[t].retval
                &&& final(store).tcb_view()[t].bind_bits == old(store).tcb_view()[t].bind_bits
                &&& final(store).tcb_view()[t].bind_slots == old(store).tcb_view()[t].bind_slots
            }
        }),
        // Dead, queue-detached TCBs are frozen across the splice:
        // a signal-shaped edit — only `t` and its chain predecessor (both `wait_notif == Some(n)`)
        // move, and `refs` drops only at `n` (which had a waiter, so `refs[n] > 0`). So a
        // `wait_notif is None`, `refs == 0` object is untouched. `destroy_tcb` reads it off for
        // its own promise about the *other* dead objects (its subject is excepted separately).
        cspace::dead_tcb_frozen(old(store), final(store)),
        // "Dead stays dead": absent ⟹ `refs` unchanged; present ⟹ only `refs[n]` (positive)
        // drops, keeping the domain — so a dead object stays dead. `destroy_tcb` composes it.
        cspace::refs_death_persist(old(store), final(store)),
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
            // residency too: the walk never touches `cspace_view`, so
            // the absent-path post-state can frame it, and `destroy_tcb` carries it forward.
            store.cspace_view() == old(store).cspace_view(),
            store.irq_view() == old(store).irq_view(),
            // The walk is read-only on the ready view too — pinned so both return paths can
            // re-establish the ready invariants (off-chain on the found path, equal on absent).
            store.ready_view() == old(store).ready_view(),
            // …and the entry-state ready invariants survive into the body (the function `requires`
            // are not visible inside the loop — part-2 technique 16), so the found-path off-chain
            // frame can cite `ready_wf(old)`/`ready_complete(old)`.
            cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
            cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
            // pin the pre-loop ghosts to the function entry state — a loop body only
            // assumes the invariant, so without this the `nv0 == old(store)...` links
            // (needed for the dom/contract postconditions at the in-loop return) are lost.
            nv0 == old(store).notif_view(),
            tv0 == old(store).tcb_view(),
            ws0 == cspace::waiter_seq(nv0, tv0, n),
            // the chain + the refs side-condition survive into the body.
            cspace::waiter_chain(nv0, tv0, n, ws0),
            nv0.dom().contains(n),
            nv0[n].wait_head is Some ==> store.refs_view().dom().contains(n) && store.refs_view()[n]
                > 0,
            // `cur`/`prev` track position `k` in `ws0`, no `t` seen yet.
            0 <= k <= ws0.len(),
            cur == (if k < ws0.len() {
                Some(ws0[k])
            } else {
                None::<ObjId>
            }),
            prev == (if k == 0 {
                None::<ObjId>
            } else {
                Some(ws0[k - 1])
            }),
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
                    store.set_notif_wait_head(n, next);  // notif insert on resident `n`
                    proof {
                        assert(store.notif_view().dom() =~= nv0.dom());
                    }
                },
                Some(p) => {
                    proof {
                        assert(k > 0);
                        assert(p == ws0[k - 1]);
                        assert(tv0.dom().contains(p));
                    }
                    store.set_tcb_qnext(p, next);  // tcb insert on resident `p`
                    proof {
                        assert(store.tcb_view().dom() =~= tv0.dom());
                    }
                },
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
                store.set_notif_wait_tail(n, prev);  // notif insert on resident `n`
                proof {
                    assert(store.notif_view().dom() =~= nv0.dom());
                }
            }
            store.set_tcb_qnext(t, None);  // tcb inserts on resident `t`
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
                cspace::lemma_waiter_chain_unique(
                    nvf,
                    tvf,
                    n,
                    cspace::waiter_seq(nvf, tvf, n),
                    ws0.remove(k),
                );
                assert(cspace::notif_wf(nvf, tvf, n));
                // The splice dequeues exactly one waiter (`t`) from `n`: `refs[n]` drops by one,
                // matched by `waiter_seq(n)` losing `t`, and every other census term is framed.
                // The per-object census map (`cspace::lemma_waiter_dequeue_census`) yields the
                // frozen delta — and below, `census_dom_complete`-preservation; the body proves
                // only the cheap local facts it keys on (§10).
                ws0.remove_ensures(k);
                assert(cspace::waiter_refs(nv0, tv0, n) == ws0.len());
                assert(cspace::waiter_refs(nvf, tvf, n) == ws0.remove(k).len());
                assert(store.refs_view().dom() == old(store).refs_view().dom());
                assert(store.refs_view() == old(store).refs_view().insert(
                    n,
                    (old(store).refs_view()[n] - 1) as nat,
                ));
                assert(nvf == nv0.insert(n, nvf[n]));
                assert forall|kk: ObjId| #[trigger] tvf[kk].cspace == tv0[kk].cspace by {}
                assert forall|kk: ObjId| #[trigger] tvf[kk].aspace == tv0[kk].aspace by {}
                // Only `t` and its chain predecessor moved, both naming `n`: `wait_notif` is
                // `Some(n)` before, and `Some(n)` or `None` (only `t` clears it) after.
                assert forall|kk: ObjId| #[trigger] tvf[kk] != tv0[kk] implies (
                tv0[kk].wait_notif is None || tv0[kk].wait_notif == Some(n)) && (
                tvf[kk].wait_notif is None || tvf[kk].wait_notif == Some(n)) by {
                    assert(tvf[kk].qnext != tv0[kk].qnext || tvf[kk].wait_notif
                        != tv0[kk].wait_notif);
                    assert(tv0[kk].wait_notif == Some(n));
                    if kk != t {
                        assert(tvf[kk].wait_notif == tv0[kk].wait_notif);
                    }
                }
                cspace::lemma_waiter_dequeue_census(old(store), store, n);
                // refcount_sound (conditional): the frozen delta the map yields bridges it.
                assert(cspace::census_delta_frozen(old(store), store));
                if cspace::refcount_sound(old(store)) {
                    cspace::lemma_refcount_sound_from_frozen(old(store), store);
                }
                // ── Teardown system invariants survive the splice
                //    (the `signal`→`fire` precedent). The splice is signal-shaped: only `n`'s
                //    notif head/tail moved, only `n`'s waiters (`t` + its predecessor) moved,
                //    and every TCB's `bind_slots`/`cspace` is fixed (the setters struct-update). ──

                assert(store.slot_view() == old(store).slot_view());
                assert(store.chan_view() == old(store).chan_view());
                assert(store.cspace_view() == old(store).cspace_view());
                assert(store.irq_view() == old(store).irq_view());
                assert(nvf =~= nv0.insert(n, nvf[n]));
                assert forall|kk: ObjId|
                    old(store).tcb_view()[kk].wait_notif != Some(n) implies #[trigger] tvf[kk]
                    == tv0[kk] by {
                    if tvf[kk] != tv0[kk] {
                        assert(tv0[kk].wait_notif == Some(n));
                    }
                }
                assert forall|kk: ObjId| #[trigger] tvf[kk].bind_slots == tv0[kk].bind_slots by {}
                assert forall|kk: ObjId| #[trigger] tvf[kk].cspace == tv0[kk].cspace by {}
                // `t`'s remaining non-queue fields are untouched (the splice writes only `t`'s
                // `qnext`/`wait_notif`), so the field-frame ensures hold.
                // Single-key asserts (not domain `forall`s) keep the hot loop body under rlimit
                // (the decomposition discipline).
                assert(tvf[t].aspace == tv0[t].aspace);
                assert(tvf[t].state == tv0[t].state);
                assert(tvf[t].report == tv0[t].report);
                assert(tvf[t].retval == tv0[t].retval);
                assert(tvf[t].bind_bits == tv0[t].bind_bits);
                // A changed TCB still blocked in the post-state is blocked on `n` (the
                // waiter-coherence frame): the only changed TCBs are `t`
                // (its `wait_notif` cleared to `None`, so not `Some(wn)`) and `t`'s predecessor
                // (only its `qnext` moved — still `wait_notif == Some(n)`, so `wn == n`).
                assert forall|kk: ObjId| #[trigger]
                    tvf[kk] != tv0[kk] && tvf[kk].state == ThreadState::BlockedNotif implies (
                tvf[kk].wait_notif matches Some(wn) ==> wn == n) by {
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
                    // The map gives `census(store,o) <= census(old,o)` everywhere, so a positive
                    // post-census `o` had a positive pre-census ⇒ it was covered; the refs domain
                    // is unchanged, so the coverage carries.
                    assert forall|o: ObjId| #[trigger]
                        cspace::obj_census(store, o) >= 1 implies store.refs_view().dom().contains(
                        o,
                    ) by {
                        assert(cspace::obj_census(old(store), o) >= 1);
                        cspace::lemma_in_refs_from_census(old(store), o);
                    }
                }
                // dead_tcb_frozen (present): only `n`'s waiters moved, and `refs` dropped only at
                // `n` (which had a waiter, so `refs[n] > 0`) — so a dead, detached object is frozen.

                assert(old(store).refs_view()[n] > 0);
                assert(store.refs_view() == old(store).refs_view().insert(
                    n,
                    (old(store).refs_view()[n] - 1) as nat,
                ));
                assert forall|x: ObjId|
                    old(store).refs_view().dom().contains(x) && old(store).refs_view()[x]
                        == 0 implies #[trigger] store.refs_view()[x] == 0 by {
                    assert(x != n);
                }
                assert forall|kk: ObjId| #[trigger]
                    store.tcb_view()[kk] == old(store).tcb_view()[kk] || old(
                        store,
                    ).tcb_view()[kk].wait_notif == Some(n) by {}
                assert(store.refs_view().dom() =~= old(store).refs_view().dom());
                assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
                cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
                // "Dead stays dead": the splice drops only `refs[n]` (positive), keeping the domain.
                cspace::lemma_refs_death_persist_dec_ref(old(store), store, n);
                // Ready invariants ride the splice — every changed node is an `n`-waiter
                // (`wait_notif == Some(n)`, hence non-Runnable by `ready_complete`), and the
                // splice writes only `qnext`/`wait_notif`, never `state`, so each changed node is
                // non-Runnable in both states. The off-chain frame then carries the pair.
                assert forall|x: ObjId| #[trigger] tvf[x] != tv0[x] implies tv0[x].state
                    != ThreadState::Runnable && tvf[x].state != ThreadState::Runnable by {
                    assert(tv0[x].wait_notif == Some(n));
                    assert(tvf[x].state == tv0[x].state);
                }
                cspace::lemma_ready_inv_frame_offchain(old(store), store);
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
        assert(store.irq_view() == old(store).irq_view());
        assert forall|o: ObjId| #[trigger]
            cspace::obj_census(store, o) == cspace::obj_census(old(store), o) by {}
        // refcount_sound (conditional): the store is unchanged, so it carries.
        assert(cspace::census_delta_frozen(old(store), store));
        if cspace::refcount_sound(old(store)) {
            cspace::lemma_refcount_sound_from_frozen(old(store), store);
        }
        // The store is unchanged, so the teardown system invariants carry trivially.

        assert(cspace::caps_consistent(old(store)) ==> cspace::caps_consistent(store));
        assert(cspace::end_caps_sound(old(store)) ==> cspace::end_caps_sound(store));
        assert(cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(store));
        // dead_tcb_frozen (absent): the store is unchanged, so it is trivially frozen.
        assert forall|kk: ObjId| #[trigger]
            store.tcb_view()[kk] == old(store).tcb_view()[kk] || old(
                store,
            ).tcb_view()[kk].wait_notif == Some(n) by {}
        assert(store.refs_view().dom() =~= old(store).refs_view().dom());
        assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
        cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
        // "Dead stays dead" (absent): the store is unchanged, so death is trivially preserved.
        cspace::lemma_refs_death_persist_from_refs_eq(old(store), store);
        // The walk is read-only (`ready_view`/`tcb_view` pinned to `old`), so the ready
        // invariants ride the no-op via the equal-views frame.
        cspace::lemma_ready_inv_frame(old(store), store);
    }
}

} // verus!
