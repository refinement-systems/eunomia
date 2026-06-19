//! Timer objects (spec §1, §3.6): a cap to program a deadline that signals
//! a bound notification. kcore owns the armed-timer *list* — insert, unlink,
//! and the expiry sweep — operating on the list head through the [`Store`]
//! seam; the head itself (`ARMED_HEAD`) is a kernel static, and the
//! generic-timer register access (`CNTVCT`/`CNTV`, the tick) stays in the
//! `kernel` crate (`kernel/src/timer.rs`). Expiry is checked on the periodic
//! tick, so deadline resolution is one tick at MVP.

// `cspace::` is referenced only from `verus!{}` spec/proof code, erased under a
// normal build — hence the allow (the lib.rs precedent).
#[allow(unused_imports)]
use crate::cspace::{self, ObjHeader};
use crate::id::ObjId;
use crate::notification;
use crate::store::Store;
use vstd::prelude::*;
// `StoreSpec` (the `external_trait_extension`) must be in scope to resolve
// `store.timer_view()`/`timer_head_view()`/… in the §4e contracts; it erases in a
// normal build, so it is otherwise unused here (the doc/results/26 §2.3 idiom).
#[allow(unused_imports)]
use crate::cspace::StoreSpec;

#[repr(C)]
pub struct TimerObj {
    pub hdr: ObjHeader,
    pub armed: bool,
    pub deadline: u64,
    pub notif: Option<ObjId>,
    pub bits: u64,
    pub next: Option<ObjId>,
}

impl TimerObj {
    /// pre:  memory at `this` writable.
    /// post: disarmed, refs = 1 (creator cap).
    pub unsafe fn init(this: *mut TimerObj) {
        this.write(TimerObj {
            hdr: ObjHeader { refs: 1 },
            armed: false,
            deadline: 0,
            notif: None,
            bits: 0,
            next: None,
        });
    }
}

verus! {

/// Disarm a timer: unlink it from the armed list and release the ref it held on
/// its bound notification (§3.6).
///
/// Verified (plan §4e, doc/results/35): the `remove_waiter` analog (doc 33) over the
/// GLOBAL armed list — singly-linked, head-only (no tail). `!armed` ⇒ a no-op; `armed`
/// ⇒ `t` is spliced out (`timer_seq` loses `t` — by `timer_wf`'s completeness an armed
/// timer is always on the list, so the walk is guaranteed to find it), the queued ref is
/// released (`refs[notif] -= 1`, the **armed-timer term** of `refcount_sound`, the
/// `binding_refs_ok` per-op-delta precedent), and `t.armed`/`t.notif`/`t.next` cleared.
/// `timer_wf` is preserved. The `armed ⇒ refs > 0` precondition (the timer holds its
/// own ref) discharges the release `-1`. The walk is read-only; the writes are on the
/// found path, which returns (the `remove_waiter` shape).
///
/// **Refcount census (D-E1).** Exports `census_delta_frozen` + conditional
/// `refcount_sound`, matching `signal`/`remove_waiter`: the `refs[notif] -= 1` release is
/// matched by `armed_timer_refs(notif)` dropping by one (`lemma_armed_timer_disarm`), so
/// the per-object `refs - census` delta is frozen. This closes the soundness chain so a
/// verified `revoke`/`delete`/`destroy_timer` after a syscall `disarm` can discharge its
/// `refcount_sound(old)` precondition.
pub fn disarm<S: Store>(store: &mut S, t: ObjId)
    requires
        old(store).timer_view().dom().contains(t),
        // The armed-timer census term is a `dom().filter().len()`, so the lockstep delta the
        // `census_delta_frozen` export rests on needs the timer arena finite (the `destroy_timer`
        // precedent, the `cap_consistent(Timer)` standing fact). Every real caller has it.
        old(store).timer_view().dom().finite(),
        cspace::timer_wf(old(store).timer_view(), old(store).timer_head_view()),
        old(store).timer_view()[t].armed ==>
            (old(store).timer_view()[t].notif matches Some(n) ==>
                old(store).refs_view().dom().contains(n) && old(store).refs_view()[n] > 0),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).timer_view().dom() == old(store).timer_view().dom(),
        cspace::timer_wf(final(store).timer_view(), final(store).timer_head_view()),
        // The refcount census moves in lockstep (D-E1, the `signal`/`remove_waiter` precedent):
        // disarming `t` drops `refs[notif]` by one and `armed_timer_refs(notif)` by one together,
        // every other census term framed (slot/chan/notif/tcb views untouched). Unconditional and
        // `requires`-free, so census-agnostic callers stay undisturbed; `destroy_timer` consumes it.
        cspace::census_delta_frozen(old(store), final(store)),
        // `refcount_sound` as a per-op contract (D-E1): a sound census in, a sound census out —
        // the frozen delta bridged by `lemma_refcount_sound_from_frozen`. Conditional, so the
        // syscall-path caller keeps no obligation while a verified caller can discharge it.
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
        // Not armed ⇒ nothing moves.
        !old(store).timer_view()[t].armed ==> {
            &&& final(store).timer_view() == old(store).timer_view()
            &&& final(store).timer_head_view() == old(store).timer_head_view()
            &&& final(store).refs_view() == old(store).refs_view()
        },
        // Armed ⇒ `t` cleared, the ref released, the rest of the list intact. Two frames
        // `check_expired` relies on: (a) every timer other than `t` and `t`'s predecessor
        // (whose old `next` was `Some(t)`) is *fully* unchanged — gives the suffix's
        // `next`-threading; (b) every timer other than `t` keeps its `armed`/`notif`/
        // `deadline`/`bits` (only the predecessor's `next` moves) — gives the suffix's and
        // the still-armed prefix's `timer_signal_ok` across the splice.
        old(store).timer_view()[t].armed ==> {
            &&& final(store).timer_view()[t].armed == false
            &&& final(store).timer_view()[t].notif is None
            &&& final(store).timer_view()[t].next is None
            &&& final(store).refs_view() == old(store).refs_view().insert(
                    old(store).timer_view()[t].notif->Some_0,
                    (old(store).refs_view()[old(store).timer_view()[t].notif->Some_0] - 1) as nat)
            &&& !cspace::timer_seq(final(store).timer_view(), final(store).timer_head_view()).contains(t)
            &&& forall|j: ObjId| #![trigger final(store).timer_view()[j]]
                    j != t && old(store).timer_view()[j].next != Some(t)
                    ==> final(store).timer_view()[j] == old(store).timer_view()[j]
            &&& forall|j: ObjId| #![trigger final(store).timer_view()[j]] j != t ==> {
                    &&& final(store).timer_view()[j].armed == old(store).timer_view()[j].armed
                    &&& final(store).timer_view()[j].notif == old(store).timer_view()[j].notif
                    &&& final(store).timer_view()[j].deadline == old(store).timer_view()[j].deadline
                    &&& final(store).timer_view()[j].bits == old(store).timer_view()[j].bits
                }
        },
{
    if !store.timer_armed(t) {
        // Not armed ⇒ a no-op: every view and `refs` is untouched, so the census is frozen.
        proof {
            assert(cspace::census_delta_frozen(old(store), store));
            if cspace::refcount_sound(old(store)) {
                cspace::lemma_refcount_sound_from_frozen(old(store), store);
            }
        }
        return;
    }
    let ghost tmv0 = old(store).timer_view();
    let ghost head0 = old(store).timer_head_view();
    let ghost ts0 = cspace::timer_seq(tmv0, head0);
    proof {
        assert(cspace::timer_chain(tmv0, head0, ts0) && cspace::timer_complete(tmv0, ts0));
        assert(tmv0[t].armed);
        assert(ts0.contains(t));
    }

    let mut cur = store.timer_armed_head();
    let mut prev: Option<ObjId> = None;
    let ghost mut k: int = 0;

    while cur.is_some()
        invariant
            store.slot_view() == old(store).slot_view(),
            store.chan_view() == old(store).chan_view(),
            store.notif_view() == old(store).notif_view(),
            store.tcb_view() == old(store).tcb_view(),
            store.cspace_view() == old(store).cspace_view(),
            store.refs_view() == old(store).refs_view(),
            store.timer_view() == tmv0,
            store.timer_head_view() == head0,
            tmv0 == old(store).timer_view(),
            head0 == old(store).timer_head_view(),
            ts0 == cspace::timer_seq(tmv0, head0),
            cspace::timer_chain(tmv0, head0, ts0),
            cspace::timer_complete(tmv0, ts0),
            // The timer arena is finite (seeded from the requires), so `lemma_armed_timer_disarm`
            // applies inside the found-path census proof.
            tmv0.dom().finite(),
            tmv0[t].armed,
            ts0.contains(t),
            tmv0[t].notif matches Some(n) ==>
                store.refs_view().dom().contains(n) && store.refs_view()[n] > 0,
            0 <= k <= ts0.len(),
            cur == (if k < ts0.len() { Some(ts0[k]) } else { None::<ObjId> }),
            prev == (if k == 0 { None::<ObjId> } else { Some(ts0[k - 1]) }),
            forall|i: int| 0 <= i < k ==> ts0[i] != t,
        decreases ts0.len() - k,
    {
        let c = cur.unwrap();
        assert(k < ts0.len());
        assert(c == ts0[k]);
        // `ObjId`'s exec `==` is external (doc 33 §2); compare the tag.
        if c.0 == t.0 {
            assert(t == ts0[k]);
            let ghost len = ts0.len() as int;
            // `cnext == tmv0[t].next` (c == t, store still pinned to old).
            let cnext = store.timer_next(c);
            assert(cnext == tmv0[t].next);

            // Re-point the head (k==0) or the predecessor (k>0) past `t`.
            match prev {
                None => {
                    store.set_timer_armed_head(cnext);
                }
                Some(p) => {
                    proof { assert(k > 0); assert(p == ts0[k - 1]); assert(tmv0.dom().contains(p)); }
                    store.set_timer_next(p, cnext);
                }
            }

            // Release the queued ref, clear `t`. `t` armed ⇒ `notif is Some` (timer_chain).
            let nopt = store.timer_notif(t);
            assert(nopt == tmv0[t].notif);
            assert(tmv0[t].notif is Some);
            if let Some(n) = nopt {
                store.set_obj_refs(n, store.obj_refs(n) - 1);
            }
            store.set_timer_notif(t, None);
            store.set_timer_armed(t, false);
            store.set_timer_next(t, None);

            proof {
                let tmvf = store.timer_view();
                let headf = store.timer_head_view();
                assert(ts0.index_of(t) == k) by {
                    let idx = ts0.index_of(t);
                    assert(0 <= idx < ts0.len() && ts0[idx] == t);
                }
                // The sets all hit resident keys (`t`, and the predecessor `p`), so the
                // domain is unchanged — needs `=~=` through the insert chain.
                assert(tmvf.dom() =~= tmv0.dom());
                cspace::lemma_timer_remove_chain(tmv0, head0, tmvf, headf, t, ts0, k);
                // Completeness: every still-armed timer was charted on `ts0` and is not
                // `t`, so it survives the splice.
                assert(cspace::timer_complete(tmvf, ts0.remove(k))) by {
                    assert forall|j: ObjId| #[trigger] tmvf.dom().contains(j) && tmvf[j].armed
                        implies ts0.remove(k).contains(j) by {
                        assert(j != t);
                        assert(tmv0[j].armed);
                        assert(ts0.contains(j));
                        cspace::lemma_seq_remove_keeps(ts0, k, j);
                    }
                }
                assert(cspace::timer_wf(tmvf, headf));
                // `t ∉ timer_seq(final)`: the unique chain is `ts0.remove(k)`, which omits `t`.
                cspace::lemma_timer_chain_unique(tmvf, headf,
                    cspace::timer_seq(tmvf, headf), ts0.remove(k));
                assert(!ts0.remove(k).contains(t)) by {
                    ts0.remove_ensures(k);
                    assert forall|i: int| 0 <= i < ts0.remove(k).len() implies
                        ts0.remove(k)[i] != t by {
                        let ii = if i < k { i } else { i + 1 };
                        assert(ts0.remove(k)[i] == ts0[ii]);
                        assert(ii != k);
                    }
                }
                // D-E1: the census moves in lockstep with `refs`. `disarm` frames the
                // slot/chan/notif/tcb views, so only `armed_timer_refs` can move;
                // `lemma_armed_timer_disarm` pins that delta to `-1` at `t`'s bound
                // notification `n`, matching the `refs[n] -= 1` released above. The additive
                // lockstep form `census_delta_frozen` then holds without assuming soundness.
                assert(store.slot_view() == old(store).slot_view());
                assert(store.chan_view() == old(store).chan_view());
                assert(store.notif_view() == old(store).notif_view());
                assert(store.tcb_view() == old(store).tcb_view());
                assert(tmv0[t].notif is Some);
                let n = tmv0[t].notif->Some_0;
                assert(store.refs_view() == old(store).refs_view().insert(
                    n, (old(store).refs_view()[n] - 1) as nat));
                assert(store.refs_view().dom() =~= old(store).refs_view().dom());
                assert(tmv0.dom().finite());
                assert forall|o: ObjId| store.refs_view().dom().contains(o) implies
                    store.refs_view()[o] + cspace::obj_census(old(store), o)
                        == old(store).refs_view()[o] + #[trigger] cspace::obj_census(store, o) by {
                    cspace::lemma_armed_timer_disarm(tmv0, tmvf, t, o);
                }
                assert(cspace::census_delta_frozen(old(store), store));
                if cspace::refcount_sound(old(store)) {
                    cspace::lemma_refcount_sound_from_frozen(old(store), store);
                }
            }
            return;
        }
        prev = cur;
        cur = store.timer_next(c);
        proof {
            k = k + 1;
        }
    }
    proof {
        assert(k == ts0.len());
        assert(!ts0.contains(t));
        assert(false);
    }
}

/// Arm (or re-arm) a timer: signal `bits` on `notif` once the counter passes
/// `deadline`. The armed timer holds a ref on the notification.
///
/// Verified (plan §4e, doc/results/35): the head-push analog of `wait`'s tail-push.
/// `disarm` first (idempotent re-arm), `+1` on the notification ref, set the fields, push
/// onto the armed list head (`timer_seq` prepend). Modeling the ref delta in body order
/// (`disarm`'s `-1` then `arm`'s `+1`) makes the **same-notif re-arm provably net-zero**
/// (the `bind_refs_post` precedent, doc 30 §2.2). `timer_wf` is preserved.
///
/// **Refcount census (D-E1).** Exports `census_delta_frozen` + conditional `refcount_sound`,
/// matching `signal`/`remove_waiter`/`disarm`. Only `t`'s `(armed, notif)` differs between
/// the entry and exit timer maps (the `disarm` predecessor splice touches only `next`), so
/// `lemma_armed_timer_retarget` reads the armed-timer census change off that one transition;
/// it cancels exactly against the `disarm`-`-1`/`arm`-`+1` refs delta (net-zero on a
/// same-notif re-arm). This closes the soundness chain so a verified caller after a syscall
/// `arm` can discharge its `refcount_sound(old)` precondition.
pub fn arm<S: Store>(store: &mut S, t: ObjId, notif: ObjId, bits: u64, deadline: u64)
    requires
        old(store).timer_view().dom().contains(t),
        // The armed-timer census term is a `dom().filter().len()`, so the lockstep delta the
        // `census_delta_frozen` export rests on needs the timer arena finite (`disarm`'s and
        // `destroy_timer`'s precedent). Every real caller has it.
        old(store).timer_view().dom().finite(),
        old(store).refs_view().dom().contains(notif),
        old(store).refs_view()[notif] < u32::MAX,
        cspace::timer_wf(old(store).timer_view(), old(store).timer_head_view()),
        // re-arm release fragment (discharges the `disarm` `-1` if `t` is already armed).
        old(store).timer_view()[t].armed ==>
            (old(store).timer_view()[t].notif matches Some(n) ==>
                old(store).refs_view().dom().contains(n) && old(store).refs_view()[n] > 0),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view().dom() == old(store).timer_view().dom(),
        cspace::timer_wf(final(store).timer_view(), final(store).timer_head_view()),
        // `t` ends armed, bound to `notif`, with the programmed deadline/bits.
        final(store).timer_view()[t].armed,
        final(store).timer_view()[t].notif == Some(notif),
        final(store).timer_view()[t].deadline == deadline,
        final(store).timer_view()[t].bits == bits,
        // The refcount census moves in lockstep (D-E1): the disarm release and the arm
        // acquire cancel against the armed-timer census change at `t`'s notification(s).
        cspace::census_delta_frozen(old(store), final(store)),
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
{
    let ghost rv0 = old(store).refs_view();
    let ghost tmv0v = old(store).timer_view();
    let ghost armed0 = tmv0v[t].armed;
    disarm(store, t);

    let ghost rv1 = store.refs_view();
    let ghost tmv1 = store.timer_view();
    let ghost head1 = store.timer_head_view();
    let ghost ts1 = cspace::timer_seq(tmv1, head1);
    proof {
        assert(cspace::timer_chain(tmv1, head1, ts1) && cspace::timer_complete(tmv1, ts1));
        // `t` is not armed post-disarm, so it is not charted on `ts1`.
        assert(!tmv1[t].armed);
        assert(!ts1.contains(t)) by {
            if ts1.contains(t) {
                let m = ts1.index_of(t);
                assert(tmv1[ts1[m]].armed);
            }
        }
        // `disarm` only decreases `refs[notif]`, so the `+1` stays in range.
        assert(store.refs_view()[notif] <= old(store).refs_view()[notif]);
    }

    store.set_obj_refs(notif, store.obj_refs(notif) + 1);
    store.set_timer_notif(t, Some(notif));
    store.set_timer_bits(t, bits);
    store.set_timer_deadline(t, deadline);
    store.set_timer_armed(t, true);
    let h = store.timer_armed_head();
    proof { assert(h == head1); }
    store.set_timer_next(t, h);
    store.set_timer_armed_head(Some(t));

    proof {
        let tmvf = store.timer_view();
        let headf = store.timer_head_view();
        // `pts == [t] ++ ts1`.
        let pts = Seq::new((ts1.len() + 1) as nat, |i: int| if i == 0 { t } else { ts1[i - 1] });
        assert(pts.len() == ts1.len() + 1);
        assert(pts[0] == t);
        assert(forall|i: int| 1 <= i < pts.len() ==> pts[i] == ts1[i - 1]);
        // `arm`'s sets after `disarm` all hit key `t` (the head set frames `timer_view`),
        // so the post-state differs from the post-`disarm` map at `t` alone.
        assert(tmvf =~= tmv1.insert(t, tmvf[t]));
        cspace::lemma_timer_push_head_chain(tmv1, head1, tmvf, headf, t, ts1, pts);
        // Completeness: every armed timer is `t` or was charted on `ts1`, all on `pts`.
        assert(cspace::timer_complete(tmvf, pts)) by {
            assert forall|j: ObjId| #[trigger] tmvf.dom().contains(j) && tmvf[j].armed
                implies pts.contains(j) by {
                if j == t {
                    assert(pts[0] == t);
                } else {
                    assert(tmv1[j].armed);
                    assert(ts1.contains(j));
                    let m = ts1.index_of(j);
                    assert(pts[m + 1] == ts1[m]);
                }
            }
        }
        assert(cspace::timer_wf(tmvf, headf));

        // D-E1 census. Arm frames slot/chan/notif/tcb, so the census differs from `old` only
        // in `armed_timer_refs`; and only `t`'s `(armed, notif)` changed in the timer map (the
        // `disarm` predecessor splice touches only `next`). `lemma_armed_timer_retarget` reads
        // the armed-timer change off `t`'s transition; it cancels the disarm-`-1`/arm-`+1` refs
        // delta exactly (net-zero on a same-notif re-arm), so `refs - census` is frozen.
        let rvf = store.refs_view();
        assert(store.slot_view() == old(store).slot_view());
        assert(store.chan_view() == old(store).chan_view());
        assert(store.notif_view() == old(store).notif_view());
        assert(store.tcb_view() == old(store).tcb_view());
        // refs = the post-disarm map `rv1` with the `+1` push at `notif`.
        assert(rvf == rv1.insert(notif, (rv1[notif] + 1) as nat));
        // `rv1` from `disarm`'s ensures (conditional on whether `t` was armed).
        assert(armed0 ==> tmv0v[t].notif is Some);
        let m = tmv0v[t].notif->Some_0;
        assert(armed0 ==> rv1 == rv0.insert(m, (rv0[m] - 1) as nat));
        assert(armed0 ==> rv0[m] > 0);
        assert(!armed0 ==> rv1 == rv0);
        // Only `t`'s `(armed, notif)` differs from `old`: `tmvf[j] == tmv1[j]` for `j != t`
        // (the head push is a single `insert` at `t`), and `disarm` framed `j`'s armed/notif.
        assert(tmvf.dom() == tmv0v.dom());
        assert forall|j: ObjId| #![trigger tmvf[j]] j != t implies
            tmvf[j].armed == tmv0v[j].armed && tmvf[j].notif == tmv0v[j].notif by {
            assert(tmvf[j] == tmv1[j]);
        }
        assert(tmvf[t].armed && tmvf[t].notif == Some(notif));
        assert(tmv0v.dom().finite());
        assert forall|o: ObjId| store.refs_view().dom().contains(o) implies
            rvf[o] + cspace::obj_census(old(store), o)
                == rv0[o] + #[trigger] cspace::obj_census(store, o) by {
            cspace::lemma_armed_timer_retarget(tmv0v, tmvf, t, o);
        }
        assert(store.refs_view().dom() == old(store).refs_view().dom());
        assert(cspace::census_delta_frozen(old(store), store));
        if cspace::refcount_sound(old(store)) {
            cspace::lemma_refcount_sound_from_frozen(old(store), store);
        }
    }
}

/// pre:  refs == 0 (last cap gone).
///
/// Verified (plan §4e, doc/results/35): teardown of a timer object — just `disarm`
/// (release the notification ref, unlink from the armed list) if it is still armed.
///
/// **Refcount census (plan §6c).** Strengthened to require and preserve
/// `refcount_sound`, so `obj_unref`'s Timer arm (6c) can conclude the invariant after
/// the dispatch. The only ref `destroy_timer` touches is `disarm`'s release of `t`'s
/// notification `n`: that `-1` is matched by `armed_timer_refs(n)` dropping by one (`t`
/// is disarmed, every other timer's `armed`/`notif` framed — `lemma_armed_timer_disarm`),
/// and `disarm` frames the slot/chan/notif/tcb views, so every other census term is
/// unchanged. The not-armed path is a no-op (`disarm` leaves the store untouched), so
/// the invariant carries trivially. The `timer_view` finiteness is the recount lemma's
/// gate; the armed-notif-live precondition (inherited from `disarm`) is the underflow
/// gate for that `-1`.
pub fn destroy_timer<S: Store>(store: &mut S, t: ObjId)
    requires
        old(store).timer_view().dom().contains(t),
        old(store).timer_view().dom().finite(),
        cspace::timer_wf(old(store).timer_view(), old(store).timer_head_view()),
        cspace::refcount_sound(old(store)),
        old(store).timer_view()[t].armed ==>
            (old(store).timer_view()[t].notif matches Some(n) ==>
                old(store).refs_view().dom().contains(n) && old(store).refs_view()[n] > 0),
        cspace::caps_consistent(old(store)),
        cspace::end_caps_sound(old(store)),
        cspace::census_dom_complete(old(store)),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).timer_view().dom() == old(store).timer_view().dom(),
        cspace::timer_wf(final(store).timer_view(), final(store).timer_head_view()),
        cspace::refcount_sound(final(store)),
        // `disarm` only lowers a census term and keeps the refs domain, so the coverage
        // carries (an object with census >= 1 in the post-state had census >= 1 before).
        cspace::census_dom_complete(final(store)),
        // `disarm` keeps the timer domain + `timer_wf` and frames every other object view,
        // so each live cap's (refs-free) consistency carries over (plan §6d).
        cspace::caps_consistent(final(store)),
        // `disarm` frames chan_view + slot_view, so the endpoint-cap census rides through
        // (plan §6d body-removal gate).
        cspace::end_caps_sound(final(store)),
        // Dead, queue-detached TCBs are frozen (plan §6d-final-thread-body): `disarm` frames
        // `tcb` whole and drops `refs` only at the armed binding's notification (which had
        // `refs > 0`), so a dead, detached object is untouched. `obj_unref`'s Timer arm reads it.
        cspace::dead_tcb_frozen(old(store), final(store)),
        // §6e-dual "dead stays dead": `disarm` keeps the refs domain and only drops the armed
        // binding's notification ref (which was positive), so a dead object stays dead.
        // `obj_unref`'s Timer arm composes this for the §6e-dual provenance frame.
        cspace::refs_death_persist(old(store), final(store)),
{
    let ghost tmv0 = old(store).timer_view();
    let ghost head0 = old(store).timer_head_view();
    let ghost armed0 = tmv0[t].armed;
    disarm(store, t);
    proof {
        let tmvf = store.timer_view();
        // disarm frames the slot/chan/notif/tcb views, so only `armed_timer_refs` can
        // move in the census; the other five terms are literally equal (same arguments).
        assert(store.slot_view() == old(store).slot_view());
        assert(store.chan_view() == old(store).chan_view());
        assert(store.notif_view() == old(store).notif_view());
        assert(store.tcb_view() == old(store).tcb_view());
        if armed0 {
            // armed ⟹ notif is Some: `t` is charted on the armed chain (completeness),
            // and every charted timer carries `notif is Some` (timer_chain).
            let ts0 = cspace::timer_seq(tmv0, head0);
            assert(cspace::timer_chain(tmv0, head0, ts0) && cspace::timer_complete(tmv0, ts0));
            assert(tmv0.dom().contains(t) && tmv0[t].armed);
            assert(ts0.contains(t));
            let i = ts0.index_of(t);
            assert(0 <= i < ts0.len() && ts0[i] == t);
            assert(tmv0[ts0[i]].notif is Some);
            let n = tmv0[t].notif->Some_0;
            // disarm's armed-case deltas (the `n` release + the armed/notif frame).
            assert(store.refs_view() == old(store).refs_view().insert(
                n, (old(store).refs_view()[n] - 1) as nat));
            assert(store.refs_view().dom() =~= old(store).refs_view().dom());
            assert(old(store).refs_view()[n] > 0);
            // dead-stays-dead (armed): `refs` moved only at `n` (`refs[n] > 0`), so a dead `x`
            // is `x != n` and keeps `refs[x] == 0` (plan §6d-final-thread-body).
            assert forall|x: ObjId|
                old(store).refs_view().dom().contains(x) && old(store).refs_view()[x] == 0
                implies #[trigger] store.refs_view()[x] == 0 by { assert(x != n); }
            // The census moves only at `n`, by exactly the `-1` that `disarm` released.
            assert forall|o: ObjId| store.refs_view().dom().contains(o)
                implies store.refs_view()[o] == cspace::obj_census(store, o) by {
                cspace::lemma_armed_timer_disarm(tmv0, tmvf, t, o);
                assert(old(store).refs_view()[o] == cspace::obj_census(old(store), o));
            }
        } else {
            // Not armed ⇒ disarm is a no-op; the store (refs + every view) is unchanged.
            assert(store.refs_view() == old(store).refs_view());
            assert forall|o: ObjId| store.refs_view().dom().contains(o)
                implies store.refs_view()[o] == cspace::obj_census(store, o) by {
                assert(cspace::obj_census(store, o) == cspace::obj_census(old(store), o));
            }
            // dead-stays-dead (not armed): `refs` is unchanged.
            assert forall|x: ObjId|
                old(store).refs_view().dom().contains(x) && old(store).refs_view()[x] == 0
                implies #[trigger] store.refs_view()[x] == 0 by {}
        }
        // caps_consistent: `disarm` frames cspace and keeps the timer domain, so the Timer
        // arm reads an unchanged domain + the ensured `timer_wf`; every other arm reads a
        // framed object view. Each live cap's consistency carries over.
        assert(store.cspace_view() == old(store).cspace_view());
        assert(store.timer_view().dom() == old(store).timer_view().dom());
        assert forall|s: crate::id::SlotId| #![trigger store.slot_view()[s]]
            store.slot_view().dom().contains(s)
                && !cspace::is_empty_cap(store.slot_view()[s].cap)
            implies cspace::cap_consistent(store, store.slot_view()[s].cap) by {
            assert(cspace::cap_consistent(old(store), old(store).slot_view()[s].cap));
        }
        // census_dom_complete: the refs domain is unchanged and the census only dropped (at
        // `n` in the armed case; unchanged otherwise), so any object with census >= 1 now had
        // census >= 1 before ⇒ it was already covered. Reuse the disarm census frame.
        assert(store.refs_view().dom() == old(store).refs_view().dom());
        assert forall|o: ObjId| #[trigger] cspace::obj_census(store, o) >= 1
            implies store.refs_view().dom().contains(o) by {
            if !store.refs_view().dom().contains(o) {
                if armed0 {
                    cspace::lemma_armed_timer_disarm(tmv0, store.timer_view(), t, o);
                }
                // census(final, o) == census(old, o), which is 0 for o ∉ dom (census_dom_complete).
                assert(cspace::obj_census(old(store), o) == 0);
            }
        }
        // dead_tcb_frozen: `tcb` is framed whole, and the dead-stays-dead refs fact was
        // established in each `armed0` branch (the only `refs` move is at the bound notification,
        // which had `refs > 0`). So a dead object stays dead and detached.
        assert forall|k: ObjId| #[trigger] store.tcb_view()[k] == old(store).tcb_view()[k]
            || old(store).tcb_view()[k].wait_notif == Some(t) by {}
        assert(store.refs_view().dom() =~= old(store).refs_view().dom());
        assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
        cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, t);
        // §6e-dual "dead stays dead": the refs domain is unchanged, and a dead in-domain object
        // (`refs == 0`) keeps `refs == 0` (the per-branch fact above) — so death is preserved.
        assert forall|o: ObjId| cspace::dead_obj(old(store), o)
            implies #[trigger] cspace::dead_obj(store, o) by {
            if old(store).refs_view().dom().contains(o) && old(store).refs_view()[o] == 0 {
                assert(store.refs_view()[o] == 0);
            }
        }
    }
}

// `timer_signal_ok` survives a `disarm(c)` + `signal(n)` (n = `c`'s notification) WITHOUT the
// distinct-notification assumption — the general N-timers→1-notification case (D-E2). For
// every *other* armed timer `cp` (bound to `np`):
//   - `np != n`: the fire touches only `n`'s notification view, the woken thread, and `n`'s
//     refs, so `np`'s liveness/`notif_wf`/refs are framed (`lemma_notif_wf_frame`) and `cp`'s
//     `timer_signal_ok_at` carries from the pre-state — the old injectivity path.
//   - `np == n` (shared notification): `cp` is still armed on `n` after the fire, so the
//     armed-timer census term `armed_timer_refs(n) >= 1`; `signal` keeps `n` well-formed; and
//     `refcount_sound` pins `refs[n] == obj_census(n)`, whose nat summands include
//     `armed_timer_refs(n)` (⇒ `refs[n] >= 1`) and, when a waiter is queued, `waiter_refs(n)`
//     (⇒ `refs[n] >= 2`). The census reconstructs exactly the refs facts injectivity used to
//     give for free — the census phase that replaces the injectivity precondition.
proof fn lemma_signal_ok_after_fire<S: Store>(
    store2: &S,
    tmv_pre: Map<ObjId, cspace::TimerView>,
    nv_pre: Map<ObjId, cspace::NotifView>,
    tv_pre: Map<ObjId, cspace::TcbView>,
    rv_pre: Map<ObjId, nat>,
    c: ObjId,
    n: ObjId,
)
    requires
        cspace::timer_signal_ok(tmv_pre, nv_pre, tv_pre, rv_pre),
        // The census is sound in the post-fire state (a maintained sweep invariant); it is what
        // reconstructs the refs facts for a notification shared by a second armed timer.
        cspace::refcount_sound(store2),
        tmv_pre.dom().contains(c),
        tmv_pre[c].armed,
        tmv_pre[c].notif == Some(n),
        // post-fire timer view: `c` unarmed, every other timer keeps armed/notif; finite (the
        // gate of the `armed_timer_refs` witness count).
        store2.timer_view().dom() == tmv_pre.dom(),
        store2.timer_view().dom().finite(),
        !store2.timer_view()[c].armed,
        forall|j: ObjId| #![trigger store2.timer_view()[j]]
            j != c ==> store2.timer_view()[j].armed == tmv_pre[j].armed
                && store2.timer_view()[j].notif == tmv_pre[j].notif,
        // notif/tcb/refs differ from the pre-state only at `n` (disarm frames notif/tcb and
        // touches refs[n]; signal touches `n`'s view, the threads waiting on `n`, refs[n]).
        store2.notif_view().dom() == nv_pre.dom(),
        forall|m: ObjId| #![trigger store2.notif_view()[m]]
            m != n ==> store2.notif_view()[m] == nv_pre[m],
        store2.tcb_view().dom() == tv_pre.dom(),
        forall|th: ObjId| #![trigger store2.tcb_view()[th]]
            tv_pre[th].wait_notif != Some(n) ==> store2.tcb_view()[th] == tv_pre[th],
        rv_pre.dom().contains(n),
        store2.refs_view().dom() == rv_pre.dom(),
        forall|m: ObjId| #![trigger store2.refs_view()[m]]
            m != n ==> store2.refs_view()[m] == rv_pre[m],
        // `n` stays well-formed across the fire (`signal` ensures it) — the well-formedness
        // half of `timer_signal_ok_at` for the shared-notification case.
        cspace::notif_wf(store2.notif_view(), store2.tcb_view(), n),
    ensures
        cspace::timer_signal_ok(store2.timer_view(), store2.notif_view(),
            store2.tcb_view(), store2.refs_view()),
{
    let tmv2 = store2.timer_view();
    let nv2 = store2.notif_view();
    let tv2 = store2.tcb_view();
    let rv2 = store2.refs_view();
    assert forall|cp: ObjId| #[trigger] tmv2.dom().contains(cp)
        implies cspace::timer_signal_ok_at(tmv2, nv2, tv2, rv2, cp) by {
        if tmv2[cp].armed && tmv2[cp].notif is Some {
            assert(cp != c);
            let np = tmv2[cp].notif->Some_0;
            assert(tmv_pre[cp].armed && tmv_pre[cp].notif == Some(np));
            assert(cspace::timer_signal_ok_at(tmv_pre, nv_pre, tv_pre, rv_pre, cp));
            if np == n {
                // Shared notification (D-E2): the census re-establishes the refs facts at `n`.
                assert(nv2.dom().contains(n));
                assert(rv2.dom().contains(n));
                // `cp` is still armed on `n`, so the armed-timer census term is positive.
                cspace::lemma_armed_timer_refs_pos(tmv2, cp, n);
                // `refcount_sound` pins refs[n] to its census; drop the four framed nat
                // summands to bound it below by the armed-timer (+ waiter) terms.
                assert(rv2[n] == cspace::obj_census(store2, n));
                assert(cspace::obj_census(store2, n)
                    >= cspace::armed_timer_refs(tmv2, n) + cspace::waiter_refs(nv2, tv2, n));
                assert(rv2[n] >= 1);
                if nv2[n].wait_head is Some {
                    cspace::lemma_waiter_refs_pos_from_head(nv2, tv2, n);
                    assert(rv2[n] >= 2);
                }
            } else {
                // Distinct notification: the fire framed `np`'s view and refs.
                assert(nv2[np] == nv_pre[np]);
                assert(rv2[np] == rv_pre[np]);
                cspace::lemma_notif_wf_frame(nv_pre, tv_pre, nv2, tv2, np);
            }
        }
    }
}

/// Tick-time expiry sweep. O(armed timers) per tick — fine at MVP scale.
///
/// Verified (plan §4e, doc/results/35): the armed-list walk that `disarm`s + `signal`s
/// every expired timer. `disarm`/`signal` both frame `slot_view`/`chan_view`, so the
/// sweep does too; `disarm` preserves `timer_wf` and `signal` frames the timer views, so
/// `timer_wf` survives the whole walk. The walk reads each timer's `next` *before* its
/// `disarm`, so it continues from a node still on the (mutated) list — the cursor tracks
/// the entry snapshot `ts0`, whose unprocessed suffix `disarm`/`signal` provably leave
/// intact. The census tension (`signal`'s `wait_head ⇒ refs > 0` across multiple fires)
/// is resolved by the **refcount-soundness** precondition (`refcount_sound`): a fire on
/// notification `n` shared by a second armed timer `cp` perturbs `n`'s refs, but `cp` keeps
/// `armed_timer_refs(n) >= 1`, so the sound census forces `refs[n] == obj_census(n) >= 1`
/// (`>= 2` once a waiter is queued) — `lemma_signal_ok_after_fire` reconstructs the carried
/// `timer_signal_ok` from the census, covering the general N-timers→1-notification
/// configuration the spec admits (D-E2), not just one-timer-per-notification.
pub fn check_expired<S: Store>(store: &mut S, now: u64)
    requires
        // The timer arena is finite (`disarm`'s precondition, the `cap_consistent(Timer)`
        // standing fact); the trusted IRQ shell that drives `check_expired` supplies it.
        old(store).timer_view().dom().finite(),
        cspace::timer_wf(old(store).timer_view(), old(store).timer_head_view()),
        // The reference census is sound (the standing system invariant the trusted IRQ shell
        // maintains): it is what keeps `timer_signal_ok` across a fire on a shared
        // notification, replacing the unrealistic distinct-notification assumption (D-E2).
        cspace::refcount_sound(old(store)),
        cspace::timer_signal_ok(old(store).timer_view(), old(store).notif_view(),
            old(store).tcb_view(), old(store).refs_view()),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        cspace::timer_wf(final(store).timer_view(), final(store).timer_head_view()),
{
    let ghost tmv0 = old(store).timer_view();
    let ghost head0 = old(store).timer_head_view();
    let ghost ts0 = cspace::timer_seq(tmv0, head0);
    proof {
        assert(cspace::timer_chain(tmv0, head0, ts0) && cspace::timer_complete(tmv0, ts0));
    }

    let mut cur = store.timer_armed_head();
    let ghost mut k: int = 0;

    while cur.is_some()
        invariant
            store.slot_view() == old(store).slot_view(),
            store.chan_view() == old(store).chan_view(),
            // Finiteness is preserved (disarm keeps the timer domain; signal frames it) and
            // is the standing precondition the in-loop `disarm` needs.
            store.timer_view().dom().finite(),
            cspace::timer_wf(store.timer_view(), store.timer_head_view()),
            // The census stays sound across each fire (`disarm`/`signal` both export the
            // conditional `refcount_sound`), feeding the next iteration's shared-notification
            // re-establishment of `timer_signal_ok` (D-E2).
            cspace::refcount_sound(store),
            cspace::timer_signal_ok(store.timer_view(), store.notif_view(),
                store.tcb_view(), store.refs_view()),
            tmv0 == old(store).timer_view(),
            head0 == old(store).timer_head_view(),
            ts0 == cspace::timer_seq(tmv0, head0),
            cspace::timer_chain(tmv0, head0, ts0),
            0 <= k <= ts0.len(),
            cur == (if k < ts0.len() { Some(ts0[k]) } else { None::<ObjId> }),
            // the unprocessed suffix retains its original armed/notif/next links.
            forall|i: int| #![trigger ts0[i]] k <= i < ts0.len() ==> {
                &&& store.timer_view().dom().contains(ts0[i])
                &&& store.timer_view()[ts0[i]].armed
                &&& store.timer_view()[ts0[i]].notif is Some
                &&& store.timer_view()[ts0[i]].next
                        == (if i + 1 < ts0.len() { Some(ts0[i + 1]) } else { None::<ObjId> })
            },
        decreases ts0.len() - k,
    {
        let c = cur.unwrap();
        assert(k < ts0.len());
        assert(c == ts0[k]);

        let ghost tmv_pre = store.timer_view();
        let ghost nv_pre = store.notif_view();
        let ghost tv_pre = store.tcb_view();
        let ghost rv_pre = store.refs_view();

        let next = store.timer_next(c);
        assert(next == (if k + 1 < ts0.len() { Some(ts0[k + 1]) } else { None::<ObjId> }));

        if store.timer_deadline(c) <= now {
            let notif = store.timer_notif(c);
            let bits = store.timer_bits(c);
            assert(notif == tmv_pre[c].notif);
            assert(notif is Some);
            // disarm precondition: `c` armed ⇒ its notif has refs > 0 (≥ 1 by signal_ok).
            assert(cspace::timer_signal_ok_at(tmv_pre, nv_pre, tv_pre, rv_pre, c));

            let ghost n = notif->Some_0;
            proof {
                // The §4e census fragment for `c`: `n` is live + `notif_wf`, with refs ≥ 1
                // (the timer's ref) and ≥ 2 when a waiter is queued.
                assert(nv_pre.dom().contains(n) && rv_pre.dom().contains(n) && rv_pre[n] >= 1);
                assert(cspace::notif_wf(nv_pre, tv_pre, n));
            }

            disarm(store, c);

            let ghost rv1 = store.refs_view();
            proof {
                // post-disarm: notif/tcb framed; refs[n] == rv_pre[n] - 1; `n` still live + wf.
                assert(store.notif_view() == nv_pre);
                assert(store.tcb_view() == tv_pre);
                assert(rv1 == rv_pre.insert(n, (rv_pre[n] - 1) as nat));
                assert(nv_pre[n].wait_head is Some ==> rv1[n] >= 1);
                // `disarm` carries the census soundness from the loop invariant (its conditional
                // `refcount_sound` ensures) — what lets `signal` carry it onward (D-E2).
                assert(cspace::refcount_sound(store));
            }

            if let Some(nn) = notif {
                assert(nn == n);
                notification::signal(store, nn, bits);

                proof {
                    let tmv2 = store.timer_view();
                    let nv2 = store.notif_view();
                    let tv2 = store.tcb_view();
                    let rv2 = store.refs_view();
                    // signal touches notif/tcb/refs only at `n` (over the post-disarm state).
                    assert(nv2.dom() == nv_pre.dom());
                    assert(forall|m: ObjId| #![trigger nv2[m]] m != n ==> nv2[m] == nv_pre[m]);
                    assert(tv2.dom() == tv_pre.dom());
                    assert(forall|th: ObjId| #![trigger tv2[th]]
                        tv_pre[th].wait_notif != Some(n) ==> tv2[th] == tv_pre[th]);
                    assert(rv2.dom() == rv_pre.dom());
                    assert(forall|m: ObjId| #![trigger rv2[m]] m != n ==> rv2[m] == rv_pre[m]);
                    // `signal` carries the census soundness from the post-disarm state (its
                    // conditional `refcount_sound` ensures) — the census the shared-notification
                    // re-establishment of `timer_signal_ok` rides on, and the maintained invariant.
                    assert(cspace::refcount_sound(store));
                    // `c` ends unarmed and `n` stays well-formed (signal's ensures); the timer
                    // arena is finite + unchanged in domain across the fire.
                    assert(store.timer_view().dom() == tmv_pre.dom());
                    assert(store.timer_view().dom().finite());
                    assert(cspace::notif_wf(nv2, tv2, n));
                    assert(tmv_pre[c].notif == Some(n));
                    // `timer_signal_ok` survives the fire — for a notification shared by a second
                    // armed timer, the sound census reconstructs the refs facts (D-E2).
                    lemma_signal_ok_after_fire(store, tmv_pre, nv_pre, tv_pre, rv_pre, c, n);
                    // The unprocessed suffix `ts0[k+1..]` is untouched: each such node is not
                    // `c` and its `next` was not `Some(c)`, so `disarm` left it whole and
                    // `signal` frames `timer_view`.
                    assert forall|i: int| #![trigger ts0[i]] k + 1 <= i < ts0.len() implies {
                        &&& tmv2.dom().contains(ts0[i])
                        &&& tmv2[ts0[i]].armed
                        &&& tmv2[ts0[i]].notif is Some
                        &&& tmv2[ts0[i]].next
                                == (if i + 1 < ts0.len() { Some(ts0[i + 1]) } else { None::<ObjId> })
                    } by {
                        assert(ts0[i] != c);
                        assert(tmv_pre[ts0[i]].next != Some(c));
                    }
                }
            }
        } else {
            // Not expired: the store is unchanged this iteration; the suffix shrinks by one.
            proof {
                assert forall|i: int| #![trigger ts0[i]] k + 1 <= i < ts0.len() implies {
                    &&& store.timer_view().dom().contains(ts0[i])
                    &&& store.timer_view()[ts0[i]].armed
                    &&& store.timer_view()[ts0[i]].notif is Some
                    &&& store.timer_view()[ts0[i]].next
                            == (if i + 1 < ts0.len() { Some(ts0[i + 1]) } else { None::<ObjId> })
                } by {}
            }
        }
        cur = next;
        proof {
            k = k + 1;
        }
    }
}

} // verus!
