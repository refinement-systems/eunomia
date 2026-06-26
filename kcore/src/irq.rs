//! IRQ-handler objects (rev2§1, rev2§3.6): a cap granting the right to receive and
//! acknowledge a device interrupt. The **census twin of the timer object** (`crate::timer`),
//! minus the armed list: an IRQ cap binds a (notification, bits) pair exactly as a timer
//! does, and a hardware interrupt signals that notification — but delivery is by direct
//! INTID→object lookup (the trusted `kernel` shell), not by sweeping a chain, so
//! there is **no armed list** here. The binding holds a ref on its notification (the
//! `irq_binding_refs` census term), so revoking the notification cap cannot free it under a
//! bound IRQ — the exact hazard `armed_timer_refs` guards for timers.
//!
//! Because there is no list, `irq_bind`/`irq_unbind`/`destroy_irq` are **single-key edits**
//! at the IRQ object: no `while`/`decreases`, no `timer_chain`/`timer_seq`/`timer_complete`
//! analogs, no splice walk. The proof is the census bookkeeping alone — the
//! `lemma_irq_binding_retarget` single-key transition discharges `bind`, `unbind`, and the
//! `destroy_irq` teardown uniformly.
// `cspace::` is referenced only from `verus!{}` spec/proof code, erased under a normal
// build — hence the allow (the `timer.rs` precedent).
#[allow(unused_imports)]
use crate::cspace::{self, IrqView, ObjHeader};
use crate::id::ObjId;
use crate::store::Store;
use vstd::prelude::*;
// `StoreSpec` (the `external_trait_extension`) must be in scope to resolve
// `store.irq_view()`/… in the verified contracts; it erases in a normal build.
#[allow(unused_imports)]
use crate::cspace::StoreSpec;

#[repr(C)]
pub struct IrqObj {
    pub hdr: ObjHeader,
    pub intid: u32,
    pub notif: Option<ObjId>,
    pub bits: u64,
    pub bound: bool,
    pub masked: bool,
}

impl IrqObj {
    /// pre:  memory at `this` writable.
    /// post: unbound, unmasked, refs = 1 (creator cap), carrying `intid`.
    pub unsafe fn init(this: *mut IrqObj, intid: u32) {
        this.write(IrqObj {
            hdr: ObjHeader { refs: 1 },
            intid,
            notif: None,
            bits: 0,
            bound: false,
            masked: false,
        });
    }

    /// Boot-static constructor: a `const` IRQ object
    /// for the kernel's fixed `IRQ_TABLE`. The `init` value as a `const fn`, so the
    /// trusted shell can place the IRQ objects in the kernel image (the device-MMIO
    /// -frame precedent) rather than retype them from untyped — no `ExIrqObj` seam.
    /// Unbound, unmasked, `refs = 1` (the init grant), carrying `intid`.
    pub const fn boot_static(intid: u32) -> IrqObj {
        IrqObj {
            hdr: ObjHeader { refs: 1 },
            intid,
            notif: None,
            bits: 0,
            bound: false,
            masked: false,
        }
    }
}

verus! {

/// Unbind an IRQ: release the ref it held on its bound notification (rev2§3.6) and clear
/// the binding. The `disarm` analog, **minus the armed-list splice** — a single-key edit at
/// `i`, so the proof is the census delta alone (`lemma_irq_binding_retarget` with `post`
/// unbound). `!bound` ⇒ a no-op; `bound` ⇒ the queued ref is released (`refs[notif] -= 1`,
/// the **irq-binding term** of `refcount_sound`) and `i.bound`/`i.notif` cleared.
///
/// **Refcount census.** Exports `census_delta_frozen` + conditional `refcount_sound`, the
/// `disarm`/`signal` precedent: the `refs[notif] -= 1` release is matched by
/// `irq_binding_refs(notif)` dropping by one, so the per-object `refs - census` delta is
/// frozen. The `bound ⇒ refs > 0` precondition discharges the release `-1`.
pub fn irq_unbind<S: Store>(store: &mut S, i: ObjId)
    requires
        old(store).irq_view().dom().contains(i),
        // The irq-binding census term is a `dom().filter().len()`, so the lockstep delta the
        // `census_delta_frozen` export rests on needs the IRQ arena finite (the `disarm` precedent,
        // the `cap_consistent(Irq)` standing fact). Every real caller has it.
        old(store).irq_view().dom().finite(),
        // `bound ⇒ notif is Some` (the `irq_wf` per-object fact) — gives `notif->Some_0` below.
        cspace::irq_wf(old(store).irq_view()),
        old(store).irq_view()[i].bound ==> (old(store).irq_view()[i].notif matches Some(n) ==> old(
            store,
        ).refs_view().dom().contains(n) && old(store).refs_view()[n] > 0),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).irq_view().dom() == old(store).irq_view().dom(),
        cspace::irq_wf(final(store).irq_view()),
        cspace::census_delta_frozen(old(store), final(store)),
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
        // Not bound ⇒ nothing moves.
        !old(store).irq_view()[i].bound ==> {
            &&& final(store).irq_view() == old(store).irq_view()
            &&& final(store).refs_view() == old(store).refs_view()
        },
        // Bound ⇒ `i` cleared, the ref released, every other IRQ fully unchanged.
        old(store).irq_view()[i].bound ==> {
            &&& final(store).irq_view()[i].bound == false
            &&& final(store).irq_view()[i].notif is None
            &&& final(store).refs_view() == old(store).refs_view().insert(
                old(store).irq_view()[i].notif->Some_0,
                (old(store).refs_view()[old(store).irq_view()[i].notif->Some_0] - 1) as nat,
            )
            &&& forall|j: ObjId|
                #![trigger final(store).irq_view()[j]]
                j != i ==> final(store).irq_view()[j] == old(store).irq_view()[j]
        },
{
    if !store.irq_bound(i) {
        // Not bound ⇒ a no-op: every view and `refs` is untouched, so the census is frozen.
        proof {
            assert(cspace::census_delta_frozen(old(store), store));
            if cspace::refcount_sound(old(store)) {
                cspace::lemma_refcount_sound_from_frozen(old(store), store);
            }
        }
        return;
    }
    let ghost irqv0 = old(store).irq_view();
    let nopt = store.irq_notif(i);
    proof {
        assert(irqv0[i].bound);
        // `irq_wf` at entry: bound ⇒ notif is Some.
        assert(irqv0[i].notif is Some);
        assert(nopt == irqv0[i].notif);
    }
    if let Some(n) = nopt {
        store.set_obj_refs(n, store.obj_refs(n) - 1);
    }
    store.set_irq_notif(i, None);
    store.set_irq_bound(i, false);

    proof {
        let irqvf = store.irq_view();
        // `irq_wf` preserved: `i` is now unbound (the implication is vacuous), every other
        // IRQ is framed (the two sets hit key `i` only).
        assert(cspace::irq_wf(irqvf)) by {
            assert forall|k: ObjId| #[trigger] irqvf.dom().contains(k) implies (irqvf[k].bound
                ==> irqvf[k].notif is Some) by {
                if k != i {
                    assert(irqvf[k] == irqv0[k]);
                }
            }
        }
        // The census moves in lockstep with `refs`. `irq_unbind` frames the
        // slot/chan/notif/tcb/timer views, so only `irq_binding_refs` can move;
        // `lemma_irq_binding_retarget` (with `post` unbound) pins that delta to `-1` at `i`'s
        // bound notification `n`, matching the `refs[n] -= 1` released above.
        assert(store.slot_view() == old(store).slot_view());
        assert(store.chan_view() == old(store).chan_view());
        assert(store.notif_view() == old(store).notif_view());
        assert(store.tcb_view() == old(store).tcb_view());
        assert(store.timer_view() == old(store).timer_view());
        let n = irqv0[i].notif->Some_0;
        assert(store.refs_view() == old(store).refs_view().insert(
            n,
            (old(store).refs_view()[n] - 1) as nat,
        ));
        assert(store.refs_view().dom() =~= old(store).refs_view().dom());
        assert(irqv0.dom().finite());
        // The two setters hit the existing key `i`, so the IRQ domain is unchanged
        // (the `lemma_irq_binding_retarget` `post.dom() == pre.dom()` precondition).
        assert(irqvf.dom() =~= irqv0.dom());
        assert forall|o: ObjId| store.refs_view().dom().contains(o) implies store.refs_view()[o]
            + cspace::obj_census(old(store), o) == old(store).refs_view()[o]
            + #[trigger] cspace::obj_census(store, o) by {
            cspace::lemma_irq_binding_retarget(irqv0, irqvf, i, o);
        }
        assert(cspace::census_delta_frozen(old(store), store));
        if cspace::refcount_sound(old(store)) {
            cspace::lemma_refcount_sound_from_frozen(old(store), store);
        }
    }
}

/// Bind (or rebind) an IRQ: signal `bits` on `notif` when the line fires (the signal itself
/// lives in the trusted delivery shell). The bound IRQ holds a ref on the
/// notification. The `arm` analog, **minus the head-push**: `irq_unbind` first (idempotent
/// rebind, net-zero on a same-notif rebind), `+1` on the notification ref, set the fields.
///
/// **Refcount census.** Exports `census_delta_frozen` + conditional `refcount_sound`. Only
/// `i`'s `(bound, notif)` differs between the entry and exit IRQ maps, so
/// `lemma_irq_binding_retarget` reads the irq-binding census change off that one transition;
/// it cancels the unbind-`-1`/bind-`+1` refs delta exactly (net-zero on a same-notif rebind).
pub fn irq_bind<S: Store>(store: &mut S, i: ObjId, notif: ObjId, bits: u64)
    requires
        old(store).irq_view().dom().contains(i),
        old(store).irq_view().dom().finite(),
        cspace::irq_wf(old(store).irq_view()),
        old(store).refs_view().dom().contains(notif),
        old(store).refs_view()[notif] < u32::MAX,
        // re-bind release fragment (discharges the `irq_unbind` `-1` if `i` is already bound).
        old(store).irq_view()[i].bound ==> (old(store).irq_view()[i].notif matches Some(n) ==> old(
            store,
        ).refs_view().dom().contains(n) && old(store).refs_view()[n] > 0),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).irq_view().dom() == old(store).irq_view().dom(),
        cspace::irq_wf(final(store).irq_view()),
        // `i` ends bound to `notif`, with the programmed bits.
        final(store).irq_view()[i].bound,
        final(store).irq_view()[i].notif == Some(notif),
        final(store).irq_view()[i].bits == bits,
        cspace::census_delta_frozen(old(store), final(store)),
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
{
    let ghost rv0 = old(store).refs_view();
    let ghost irqv0v = old(store).irq_view();
    let ghost bound0 = irqv0v[i].bound;
    irq_unbind(store, i);

    let ghost rv1 = store.refs_view();
    proof {
        // `irq_unbind` only decreases `refs[notif]`, so the `+1` stays in range.
        assert(store.refs_view()[notif] <= old(store).refs_view()[notif]);
        assert(!store.irq_view()[i].bound);
    }

    store.set_obj_refs(notif, store.obj_refs(notif) + 1);
    store.set_irq_notif(i, Some(notif));
    store.set_irq_bits(i, bits);
    store.set_irq_bound(i, true);

    proof {
        let irqvf = store.irq_view();
        // `irq_wf` preserved: `i` ends bound-with-`Some(notif)`; every other IRQ is framed.
        assert(cspace::irq_wf(irqvf)) by {
            assert forall|k: ObjId| #[trigger] irqvf.dom().contains(k) implies (irqvf[k].bound
                ==> irqvf[k].notif is Some) by {
                if k != i {
                    assert(irqvf[k] == irqv0v[k]);
                }
            }
        }
        // Census. `irq_bind` frames slot/chan/notif/tcb/timer, so the census differs from
        // `old` only in `irq_binding_refs`; and only `i`'s `(bound, notif)` changed in the IRQ
        // map. `lemma_irq_binding_retarget` reads the change off `i`'s transition; it cancels
        // the unbind-`-1`/bind-`+1` refs delta exactly, so `refs - census` is frozen.
        let rvf = store.refs_view();
        assert(store.slot_view() == old(store).slot_view());
        assert(store.chan_view() == old(store).chan_view());
        assert(store.notif_view() == old(store).notif_view());
        assert(store.tcb_view() == old(store).tcb_view());
        assert(store.timer_view() == old(store).timer_view());
        // refs = the post-unbind map `rv1` with the `+1` at `notif`.
        assert(rvf == rv1.insert(notif, (rv1[notif] + 1) as nat));
        // `rv1` from `irq_unbind`'s ensures (conditional on whether `i` was bound).
        assert(bound0 ==> irqv0v[i].notif is Some);
        let m = irqv0v[i].notif->Some_0;
        assert(bound0 ==> rv1 == rv0.insert(m, (rv0[m] - 1) as nat));
        assert(bound0 ==> rv0[m] > 0);
        assert(!bound0 ==> rv1 == rv0);
        // Only `i`'s `(bound, notif)` differs from `old`: `irqvf[j] == irqv0v[j]` for `j != i`
        // (`irq_unbind` framed `j`, and `bind`'s sets all hit key `i`).
        assert(irqvf.dom() == irqv0v.dom());
        assert forall|j: ObjId| #![trigger irqvf[j]] j != i implies irqvf[j].bound
            == irqv0v[j].bound && irqvf[j].notif == irqv0v[j].notif by {
            assert(irqvf[j] == irqv0v[j]);
        }
        assert(irqvf[i].bound && irqvf[i].notif == Some(notif));
        assert(irqv0v.dom().finite());
        assert forall|o: ObjId| store.refs_view().dom().contains(o) implies rvf[o]
            + cspace::obj_census(old(store), o) == rv0[o] + #[trigger] cspace::obj_census(
            store,
            o,
        ) by {
            cspace::lemma_irq_binding_retarget(irqv0v, irqvf, i, o);
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
/// Teardown of an IRQ object — just `irq_unbind` (release the notification ref, clear the
/// binding) if it is still bound. The `destroy_timer` analog, minus the chain reasoning.
///
/// **Refcount census.** Requires and preserves `refcount_sound`, so `obj_unref`'s Irq arm
/// can conclude the invariant after the dispatch. The only ref `destroy_irq` touches is
/// `irq_unbind`'s release of `i`'s notification `n`; that `-1` is matched by
/// `irq_binding_refs(n)` dropping by one, and every other census term is framed.
pub fn destroy_irq<S: Store>(store: &mut S, i: ObjId)
    requires
        old(store).irq_view().dom().contains(i),
        old(store).irq_view().dom().finite(),
        cspace::irq_wf(old(store).irq_view()),
        cspace::refcount_sound(old(store)),
        old(store).irq_view()[i].bound ==> (old(store).irq_view()[i].notif matches Some(n) ==> old(
            store,
        ).refs_view().dom().contains(n) && old(store).refs_view()[n] > 0),
        cspace::caps_consistent(old(store)),
        cspace::end_caps_sound(old(store)),
        cspace::census_dom_complete(old(store)),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).irq_view().dom() == old(store).irq_view().dom(),
        cspace::irq_wf(final(store).irq_view()),
        cspace::refcount_sound(final(store)),
        cspace::census_dom_complete(final(store)),
        cspace::caps_consistent(final(store)),
        cspace::end_caps_sound(final(store)),
        cspace::dead_tcb_frozen(old(store), final(store)),
        cspace::refs_death_persist(old(store), final(store)),
{
    let ghost irqv0 = old(store).irq_view();
    let ghost bound0 = irqv0[i].bound;
    irq_unbind(store, i);
    proof {
        let irqvf = store.irq_view();
        // `irq_unbind` frames slot/chan/notif/tcb/timer, so only `irq_binding_refs` can move
        // in the census; the other six terms are literally equal (same arguments).
        assert(store.slot_view() == old(store).slot_view());
        assert(store.chan_view() == old(store).chan_view());
        assert(store.notif_view() == old(store).notif_view());
        assert(store.tcb_view() == old(store).tcb_view());
        assert(store.timer_view() == old(store).timer_view());
        // refcount_sound carries from `irq_unbind`'s conditional ensures.
        assert(cspace::refcount_sound(store));
        if bound0 {
            // bound ⟹ notif is Some (irq_wf), and `n` is the released notification.
            assert(irqv0[i].notif is Some);
            let n = irqv0[i].notif->Some_0;
            assert(store.refs_view() == old(store).refs_view().insert(
                n,
                (old(store).refs_view()[n] - 1) as nat,
            ));
            assert(store.refs_view().dom() =~= old(store).refs_view().dom());
            assert(old(store).refs_view()[n] > 0);
            // dead-stays-dead (bound): `refs` moved only at `n` (`refs[n] > 0`), so a dead `x`
            // is `x != n` and keeps `refs[x] == 0`.
            assert forall|x: ObjId|
                old(store).refs_view().dom().contains(x) && old(store).refs_view()[x]
                    == 0 implies #[trigger] store.refs_view()[x] == 0 by {
                assert(x != n);
            }
        } else {
            // Not bound ⇒ `irq_unbind` is a no-op; the store (refs + every view) is unchanged.
            assert(store.refs_view() == old(store).refs_view());
            assert forall|x: ObjId|
                old(store).refs_view().dom().contains(x) && old(store).refs_view()[x]
                    == 0 implies #[trigger] store.refs_view()[x] == 0 by {}
        }
        // caps_consistent: `irq_unbind` frames cspace and keeps the IRQ domain + `irq_wf`, so
        // the Irq arm reads an unchanged domain + the ensured `irq_wf`; every other arm reads
        // a framed object view. Each live cap's consistency carries over.
        assert(store.cspace_view() == old(store).cspace_view());
        assert(store.irq_view().dom() == old(store).irq_view().dom());
        assert forall|s: crate::id::SlotId|
            #![trigger store.slot_view()[s]]
            store.slot_view().dom().contains(s) && !cspace::is_empty_cap(
                store.slot_view()[s].cap,
            ) implies cspace::cap_consistent(store, store.slot_view()[s].cap) by {
            assert(cspace::cap_consistent(old(store), old(store).slot_view()[s].cap));
        }
        // census_dom_complete: the refs domain is unchanged and the census only dropped (at
        // `n` in the bound case; unchanged otherwise), so any object with census >= 1 now had
        // census >= 1 before ⇒ it was already covered. Reuse the unbind census frame.
        assert(store.refs_view().dom() == old(store).refs_view().dom());
        assert forall|o: ObjId| #[trigger]
            cspace::obj_census(store, o) >= 1 implies store.refs_view().dom().contains(o) by {
            if !store.refs_view().dom().contains(o) {
                cspace::lemma_irq_binding_retarget(irqv0, store.irq_view(), i, o);
                // census(final, o) == census(old, o), which is 0 for o ∉ dom (census_dom_complete).
                assert(cspace::obj_census(old(store), o) == 0);
            }
        }
        // dead_tcb_frozen: `tcb` is framed whole, and the dead-stays-dead refs fact was
        // established in each branch (the only `refs` move is at the bound notification, which
        // had `refs > 0`). So a dead object stays dead and detached.
        assert(store.refs_view().dom() =~= old(store).refs_view().dom());
        assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
        cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, i);
        // "Dead stays dead": the refs domain is unchanged, and a dead in-domain object
        // keeps `refs == 0` (the per-branch fact above) — so death is preserved.
        assert forall|o: ObjId| cspace::dead_obj(old(store), o) implies #[trigger] cspace::dead_obj(
            store,
            o,
        ) by {
            if old(store).refs_view().dom().contains(o) && old(store).refs_view()[o] == 0 {
                assert(store.refs_view()[o] == 0);
            }
        }
    }
}

} // verus!
