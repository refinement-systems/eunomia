//! The 32-level ready queue (rev1§5.4): strict fixed-priority, round-robin within a
//! level. kcore owns the list *logic* — enqueue (append-to-tail), dequeue (pop-head),
//! unqueue (arbitrary-position splice), and the `top_ready` bit-scan — operating on the
//! per-level head/tail and the `u32` presence bitmap through the [`Store`] seam; the
//! backing (`READY`/`READY_BITMAP`) is a kernel static and the context switch
//! (`maybe_switch`) stays in the `kernel` crate (rev1§6.1(d): the scheduler *policy* and
//! the asm switch are trusted; this module is the *data structure*).
//!
//! The list is the waiter-queue shape per level (`cspace::ready_chain`/`ready_seq`), with
//! the timer-list completeness discipline (`cspace::ready_complete`) and a bitmap
//! coherence invariant (`cspace::ready_bitmap_coherent`). The intrusive link is
//! `Tcb.qnext`, shared with the notification waiter queue and disambiguated by state:
//! a thread is on the ready chain (`Runnable`) or a waiter chain (`BlockedNotif`),
//! never both.

// `cspace::` is referenced from `verus!{}` spec/proof code, erased under a normal build.
#[allow(unused_imports)]
use crate::cspace::{self, ObjHeader};
use crate::id::ObjId;
use crate::store::Store;
use crate::sysabi::NUM_PRIOS;
use crate::thread::ThreadState;
use vstd::prelude::*;
// `StoreSpec` (the `external_trait_extension`) must be in scope to resolve
// `store.ready_view()`/`tcb_view()`/… in the verified contracts.
#[allow(unused_imports)]
use crate::cspace::StoreSpec;

verus! {

// `(x >> k) & 1 != 0` (the form `axiom_u32_leading_zeros` speaks) is the bit-`k`-set test
// `x & (1 << k) != 0` (the form `ready_bitmap_coherent` speaks). A pure bit-vector fact.
pub proof fn lemma_bit_set_eqv(x: u32, k: u32)
    requires
        k < 32,
    ensures
        ((x >> k) & 1u32 != 0u32) == (x & (1u32 << k) != 0u32),
{
    assert(((x >> k) & 1u32 != 0u32) == (x & (1u32 << k) != 0u32)) by (bit_vector)
        requires k < 32;
}

// Setting bit `k` makes bit `k` set (`ready_enqueue`'s `| (1<<lvl)`).
pub proof fn lemma_set_bit_self(x: u32, k: u32)
    requires
        k < 32,
    ensures
        (x | (1u32 << k)) & (1u32 << k) != 0u32,
{
    assert((x | (1u32 << k)) & (1u32 << k) != 0u32) by (bit_vector) requires k < 32;
}

// Clearing bit `k` makes bit `k` clear (`ready_dequeue`/`ready_unqueue`'s `& !(1<<lvl)`).
pub proof fn lemma_clear_bit_self(x: u32, k: u32)
    requires
        k < 32,
    ensures
        (x & !(1u32 << k)) & (1u32 << k) == 0u32,
{
    assert((x & !(1u32 << k)) & (1u32 << k) == 0u32) by (bit_vector) requires k < 32;
}

// A set/clear of bit `k` leaves every *other* bit `j != k` untouched (instantiated per
// `j` inside the ops' bitmap-coherence re-establishment, so no forall-over-bits trigger).
pub proof fn lemma_set_bit_other(x: u32, k: u32, j: u32)
    requires
        k < 32,
        j < 32,
        j != k,
    ensures
        ((x | (1u32 << k)) & (1u32 << j) != 0u32) == (x & (1u32 << j) != 0u32),
{
    assert(((x | (1u32 << k)) & (1u32 << j) != 0u32) == (x & (1u32 << j) != 0u32)) by (bit_vector)
        requires j < 32, k < 32, j != k;
}

pub proof fn lemma_clear_bit_other(x: u32, k: u32, j: u32)
    requires
        k < 32,
        j < 32,
        j != k,
    ensures
        ((x & !(1u32 << k)) & (1u32 << j) != 0u32) == (x & (1u32 << j) != 0u32),
{
    assert(((x & !(1u32 << k)) & (1u32 << j) != 0u32) == (x & (1u32 << j) != 0u32)) by (bit_vector)
        requires j < 32, k < 32, j != k;
}

// Highest non-empty priority level, or `None` if the queue is empty. The verified
// `leading_zeros` bit-scan: `None` iff the bitmap is 0; else `31 - leading_zeros`, proven
// (via `axiom_u32_leading_zeros` + bitmap coherence) to be a non-empty level with every
// higher level empty — exactly the strict-priority pick `maybe_switch` makes.
pub fn top_ready<S: Store>(store: &S) -> (r: Option<usize>)
    requires
        cspace::ready_wf(store.ready_view(), store.tcb_view()),
    ensures
        r is None ==> store.ready_view().bitmap == 0,
        r matches Some(lvl) ==> {
            &&& lvl < NUM_PRIOS
            &&& cspace::ready_seq(store.ready_view(), store.tcb_view(), lvl as int).len() > 0
            &&& forall|j: int| #![trigger cspace::ready_seq(store.ready_view(), store.tcb_view(), j)]
                    lvl < j < NUM_PRIOS ==>
                    cspace::ready_seq(store.ready_view(), store.tcb_view(), j).len() == 0
        },
{
    broadcast use vstd::std_specs::bits::axiom_u32_leading_zeros;
    let bm = store.ready_bitmap();
    if bm == 0 {
        None
    } else {
        // axiom: bm != 0 ⇒ lz < 32 ⇒ 31 - lz ∈ 0..32 is the top set bit.
        assert(bm.leading_zeros() < 32);
        let lz = bm.leading_zeros();
        let lvl: u32 = 31 - lz;
        proof {
            assert((bm >> lvl) & 1u32 != 0u32);
            lemma_bit_set_eqv(bm, lvl);
            assert(bm & (1u32 << lvl) != 0u32);
            // every level above `lvl` has a clear bit ⇒ (coherence) an empty chain.
            assert forall|j: int| lvl < j < NUM_PRIOS implies
                #[trigger] cspace::ready_seq(store.ready_view(), store.tcb_view(), j).len() == 0 by {
                let ju = j as u32;
                assert((bm >> ju) & 1u32 == 0u32);
                lemma_bit_set_eqv(bm, ju);
                assert(bm & (1u32 << ju) == 0u32);
            }
        }
        Some(lvl as usize)
    }
}

// Bitmap coherence after setting bit `level` (`ready_enqueue`): bit `level` is now set
// and level non-empty; every other bit and chain is unchanged. The 32 per-bit
// `bit_vector` instantiations, spun off so the main sweep stays in budget.
#[verifier::spinoff_prover]
#[verifier::rlimit(100)]
pub proof fn lemma_ready_coherent_after_set(
    rv0: cspace::ReadyView,
    tv0: Map<ObjId, cspace::TcbView>,
    rvf: cspace::ReadyView,
    tvf: Map<ObjId, cspace::TcbView>,
    level: int,
    pws: Seq<ObjId>,
)
    requires
        cspace::ready_bitmap_coherent(rv0, tv0),
        0 <= level < NUM_PRIOS,
        rvf.bitmap == rv0.bitmap | (1u32 << (level as u32)),
        cspace::ready_seq(rvf, tvf, level) == pws,
        pws.len() > 0,
        forall|l: int| 0 <= l < NUM_PRIOS && l != level
            ==> #[trigger] cspace::ready_seq(rvf, tvf, l) == cspace::ready_seq(rv0, tv0, l),
    ensures
        cspace::ready_bitmap_coherent(rvf, tvf),
{
    assert forall|l: int| 0 <= l < NUM_PRIOS implies
        ((rvf.bitmap & (1u32 << (l as u32))) != 0
            <==> (#[trigger] cspace::ready_seq(rvf, tvf, l)).len() > 0) by {
        if l == level {
            assert(l as u32 == level as u32);
            lemma_set_bit_self(rv0.bitmap, level as u32);
            assert((rvf.bitmap & (1u32 << (l as u32))) != 0);
            assert(cspace::ready_seq(rvf, tvf, l) == pws);
            assert(cspace::ready_seq(rvf, tvf, l).len() > 0);
        } else {
            assert(l as u32 != level as u32);
            lemma_set_bit_other(rv0.bitmap, level as u32, l as u32);
            assert((rvf.bitmap & (1u32 << (l as u32)) != 0)
                == (rv0.bitmap & (1u32 << (l as u32)) != 0));
            assert(cspace::ready_seq(rvf, tvf, l) == cspace::ready_seq(rv0, tv0, l));
            // rv0 coherence (trigger ready_seq(rv0,tv0,l)) closes the iff.
            assert((rv0.bitmap & (1u32 << (l as u32)) != 0)
                == (cspace::ready_seq(rv0, tv0, l).len() > 0));
        }
    }
}

// Bitmap coherence after *clearing* bit `level` (`ready_dequeue`/`ready_unqueue`, when the
// level empties): bit `level` is now clear and level empty; every other bit and chain is
// unchanged. The clear-bit twin of `lemma_ready_coherent_after_set` — the level-still-
// non-empty case keeps the bitmap unchanged and is handled inline in `lemma_ready_remove_wf`.
#[verifier::spinoff_prover]
#[verifier::rlimit(100)]
pub proof fn lemma_ready_coherent_after_clear(
    rv0: cspace::ReadyView,
    tv0: Map<ObjId, cspace::TcbView>,
    rvf: cspace::ReadyView,
    tvf: Map<ObjId, cspace::TcbView>,
    level: int,
)
    requires
        cspace::ready_bitmap_coherent(rv0, tv0),
        0 <= level < NUM_PRIOS,
        rvf.bitmap == rv0.bitmap & !(1u32 << (level as u32)),
        cspace::ready_seq(rvf, tvf, level).len() == 0,
        forall|l: int| 0 <= l < NUM_PRIOS && l != level
            ==> #[trigger] cspace::ready_seq(rvf, tvf, l) == cspace::ready_seq(rv0, tv0, l),
    ensures
        cspace::ready_bitmap_coherent(rvf, tvf),
{
    assert forall|l: int| 0 <= l < NUM_PRIOS implies
        ((rvf.bitmap & (1u32 << (l as u32))) != 0
            <==> (#[trigger] cspace::ready_seq(rvf, tvf, l)).len() > 0) by {
        if l == level {
            assert(l as u32 == level as u32);
            lemma_clear_bit_self(rv0.bitmap, level as u32);
            assert((rvf.bitmap & (1u32 << (l as u32))) == 0);
            assert(cspace::ready_seq(rvf, tvf, l).len() == 0);
        } else {
            assert(l as u32 != level as u32);
            lemma_clear_bit_other(rv0.bitmap, level as u32, l as u32);
            assert((rvf.bitmap & (1u32 << (l as u32)) != 0)
                == (rv0.bitmap & (1u32 << (l as u32)) != 0));
            assert(cspace::ready_seq(rvf, tvf, l) == cspace::ready_seq(rv0, tv0, l));
            // rv0 coherence (trigger ready_seq(rv0,tv0,l)) closes the iff.
            assert((rv0.bitmap & (1u32 << (l as u32)) != 0)
                == (cspace::ready_seq(rv0, tv0, l).len() > 0));
        }
    }
}

// The post-condition sweep for an append-to-tail (`ready_enqueue`/`make_runnable`):
// from the *local* facts the op body establishes (the pushed chain at `level`, the
// per-level head/tail/bitmap frame, the tcb frame) plus the pre-state invariants, derive
// the *global* `ready_wf`/`ready_complete` over all 32 levels. Spun off into its own Z3
// instance so the op body stays within budget.
#[verifier::spinoff_prover]
#[verifier::rlimit(150)]
pub proof fn lemma_ready_push_wf(
    rv0: cspace::ReadyView,
    tv0: Map<ObjId, cspace::TcbView>,
    rvf: cspace::ReadyView,
    tvf: Map<ObjId, cspace::TcbView>,
    level: int,
    t: ObjId,
    pws: Seq<ObjId>,
)
    requires
        cspace::ready_wf(rv0, tv0),
        cspace::ready_complete(rv0, tv0),
        0 <= level < NUM_PRIOS,
        tv0.dom().contains(t),
        tv0[t].priority as int == level,
        tv0[t].state != ThreadState::Runnable,
        tvf.dom() == tv0.dom(),
        tvf[t].state == ThreadState::Runnable,
        tvf[t].priority as int == level,
        pws == cspace::ready_seq(rv0, tv0, level).push(t),
        cspace::ready_chain(rvf, tvf, level, pws),
        rvf.heads.dom() == rv0.heads.dom(),
        rvf.tails.dom() == rv0.tails.dom(),
        forall|l: int| #![trigger rvf.heads[l]] #![trigger rvf.tails[l]]
            0 <= l < NUM_PRIOS && l != level
            ==> rvf.heads[l] == rv0.heads[l] && rvf.tails[l] == rv0.tails[l],
        rvf.bitmap == rv0.bitmap | (1u32 << (level as u32)),
        forall|x: ObjId| x != t && rv0.tails[level] != Some(x) ==> #[trigger] tvf[x] == tv0[x],
        rv0.tails[level] matches Some(y) ==> tv0[y].priority as int == level,
    ensures
        cspace::ready_wf(rvf, tvf),
        cspace::ready_complete(rvf, tvf),
        cspace::ready_seq(rvf, tvf, level) == pws,
{
    // level's seq is the pushed chain.
    cspace::lemma_ready_chain_unique(rvf, tvf, level, cspace::ready_seq(rvf, tvf, level), pws);

    // other levels: seq + chain preserved.
    assert forall|l: int| 0 <= l < NUM_PRIOS && l != level implies
        #[trigger] cspace::ready_seq(rvf, tvf, l) == cspace::ready_seq(rv0, tv0, l)
        && cspace::ready_chain(rvf, tvf, l, cspace::ready_seq(rvf, tvf, l)) by {
        assert(rvf.heads[l] == rv0.heads[l] && rvf.tails[l] == rv0.tails[l]);
        let rl = cspace::ready_seq(rv0, tv0, l);
        assert(cspace::ready_chain(rv0, tv0, l, rl));
        assert forall|i: int| 0 <= i < rl.len() implies #[trigger] tvf[rl[i]] == tv0[rl[i]] by {
            assert(tv0[rl[i]].priority as int == l);
            assert(rl[i] != t);
            assert(rv0.tails[level] != Some(rl[i]));
        }
        cspace::lemma_ready_seq_frame(rv0, tv0, rvf, tvf, l);
    }

    // ready_wf assembly.
    assert(rvf.heads.dom() == Set::new(|i: int| 0 <= i < NUM_PRIOS as int));
    assert(rvf.tails.dom() == Set::new(|i: int| 0 <= i < NUM_PRIOS as int));
    assert forall|lv: int| #![trigger rvf.heads[lv]] 0 <= lv < NUM_PRIOS as int implies
        (rvf.heads[lv] is None <==> rvf.tails[lv] is None) by {
        if lv == level {
            assert(rvf.heads[level] is Some && rvf.tails[level] is Some);
        } else {
            assert(rvf.heads[lv] == rv0.heads[lv] && rvf.tails[lv] == rv0.tails[lv]);
        }
    }
    assert(cspace::ready_seq(rvf, tvf, level) == pws);
    assert forall|lv: int| #![trigger cspace::ready_seq(rvf, tvf, lv)] 0 <= lv < NUM_PRIOS as int implies
        cspace::ready_chain(rvf, tvf, lv, cspace::ready_seq(rvf, tvf, lv)) by {
        if lv != level {
            assert(cspace::ready_seq(rvf, tvf, lv) == cspace::ready_seq(rv0, tv0, lv));
        }
    }
    lemma_ready_coherent_after_set(rv0, tv0, rvf, tvf, level, pws);
    assert(cspace::ready_wf(rvf, tvf));

    // ready_complete. Case on whether `x` is on level's (pushed) chain: if so its
    // covenant comes from `ready_chain(rvf,tvf,level,pws)` (covers `t` and the old tail,
    // whose qnext moved); otherwise `x` is off level's chain, hence framed, and stays on
    // its own (untouched) level's chain.
    assert(cspace::ready_seq(rvf, tvf, level) == pws);
    assert(pws.contains(t)) by { assert(pws[pws.len() - 1] == t); }
    assert forall|x: ObjId| #[trigger] tvf.dom().contains(x) && tvf[x].state == ThreadState::Runnable
        implies (tvf[x].priority as int) < NUM_PRIOS
            && cspace::ready_seq(rvf, tvf, tvf[x].priority as int).contains(x) by {
        if pws.contains(x) {
            let k = pws.index_of(x);
            assert(0 <= k < pws.len() && pws[k] == x);
            assert(tvf[pws[k]].priority as int == level);   // ready_chain covenant
        } else {
            // x off level's chain ⇒ x != t and x != old tail (both on pws) ⇒ framed.
            assert(x != t);
            assert(rv0.tails[level] != Some(x)) by {
                if rv0.tails[level] == Some(x) {
                    let rs0 = cspace::ready_seq(rv0, tv0, level);
                    assert(cspace::ready_chain(rv0, tv0, level, rs0));
                    assert(rs0.len() > 0 && rs0[rs0.len() - 1] == x);
                    assert(pws[rs0.len() - 1] == x);   // pws == rs0.push(t)
                }
            }
            assert(tvf[x] == tv0[x]);
            let lx = tv0[x].priority as int;
            assert(lx < NUM_PRIOS && cspace::ready_seq(rv0, tv0, lx).contains(x));
            assert(lx != level) by {
                if lx == level {
                    let rs0 = cspace::ready_seq(rv0, tv0, level);
                    assert(rs0.contains(x));
                    let j = rs0.index_of(x);
                    assert(0 <= j < rs0.len() && rs0[j] == x);
                    assert(pws[j] == x && 0 <= j < pws.len());   // pws == rs0.push(t) ⇒ x ∈ pws
                    assert(pws.contains(x));
                }
            }
            assert(cspace::ready_seq(rvf, tvf, lx) == cspace::ready_seq(rv0, tv0, lx));
        }
    }
}

// The post-condition sweep for a *removal* (`ready_dequeue`/`ready_unqueue`): from the
// local facts the op body establishes (the spliced chain at `level` — `rs0.remove(k)` —, the
// per-level head/tail/bitmap frame, the tcb frame) plus the pre-state `ready_wf`, derive the
// global `ready_wf` over all 32 levels. The removal analogue of `lemma_ready_push_wf`; it
// re-establishes `ready_wf` *only* (the liveness half — full `ready_complete` vs.
// `ready_complete_except` — is the op-specific concern the caller proves). Requires only
// `ready_wf(rv0)` (not `ready_complete`), so both ops call it. Spun off into its own Z3
// instance so the op body stays within budget.
#[verifier::spinoff_prover]
#[verifier::rlimit(150)]
pub proof fn lemma_ready_remove_wf(
    rv0: cspace::ReadyView,
    tv0: Map<ObjId, cspace::TcbView>,
    rvf: cspace::ReadyView,
    tvf: Map<ObjId, cspace::TcbView>,
    level: int,
    t: ObjId,
    rs0: Seq<ObjId>,
    k: int,
)
    requires
        cspace::ready_wf(rv0, tv0),
        0 <= level < NUM_PRIOS,
        rs0 == cspace::ready_seq(rv0, tv0, level),
        0 <= k < rs0.len(),
        rs0[k] == t,
        tv0[t].priority as int == level,
        cspace::ready_chain(rvf, tvf, level, rs0.remove(k)),
        tvf.dom() == tv0.dom(),
        rvf.heads.dom() == rv0.heads.dom(),
        rvf.tails.dom() == rv0.tails.dom(),
        forall|l: int| #![trigger rvf.heads[l]] #![trigger rvf.tails[l]]
            0 <= l < NUM_PRIOS && l != level
            ==> rvf.heads[l] == rv0.heads[l] && rvf.tails[l] == rv0.tails[l],
        rs0.remove(k).len() > 0 ==> rvf.bitmap == rv0.bitmap,
        rs0.remove(k).len() == 0 ==> rvf.bitmap == rv0.bitmap & !(1u32 << (level as u32)),
        forall|x: ObjId| x != t && (k == 0 || x != rs0[k - 1]) ==> #[trigger] tvf[x] == tv0[x],
        k > 0 ==> tvf[rs0[k - 1]].state == tv0[rs0[k - 1]].state,
        k > 0 ==> tvf[rs0[k - 1]].priority == tv0[rs0[k - 1]].priority,
    ensures
        cspace::ready_wf(rvf, tvf),
        cspace::ready_seq(rvf, tvf, level) == rs0.remove(k),
        // other levels' chains are untouched — `ready_unqueue` needs this to carry
        // `ready_complete_except` for the surviving Runnable threads at other levels.
        forall|l: int| #![trigger cspace::ready_seq(rvf, tvf, l)]
            0 <= l < NUM_PRIOS && l != level
            ==> cspace::ready_seq(rvf, tvf, l) == cspace::ready_seq(rv0, tv0, l),
{
    // `rs0` is genuinely the pre-state chain at `level`; `rs0[k-1]` (the spliced node's
    // predecessor) is therefore at `level` too.
    assert(cspace::ready_chain(rv0, tv0, level, rs0));

    // level's seq is the spliced chain.
    cspace::lemma_ready_chain_unique(rvf, tvf, level, cspace::ready_seq(rvf, tvf, level),
        rs0.remove(k));

    // other levels: seq + chain preserved (their nodes have priority != level, and the only
    // moved tcbs — `t` and its predecessor `rs0[k-1]` — are at `level`, so they are framed).
    assert forall|l: int| 0 <= l < NUM_PRIOS && l != level implies
        #[trigger] cspace::ready_seq(rvf, tvf, l) == cspace::ready_seq(rv0, tv0, l)
        && cspace::ready_chain(rvf, tvf, l, cspace::ready_seq(rvf, tvf, l)) by {
        assert(rvf.heads[l] == rv0.heads[l] && rvf.tails[l] == rv0.tails[l]);
        let rl = cspace::ready_seq(rv0, tv0, l);
        assert(cspace::ready_chain(rv0, tv0, l, rl));
        assert forall|i: int| 0 <= i < rl.len() implies #[trigger] tvf[rl[i]] == tv0[rl[i]] by {
            assert(tv0[rl[i]].priority as int == l);
            assert(rl[i] != t);
            if k > 0 {
                assert(tv0[rs0[k - 1]].priority as int == level);
                assert(rl[i] != rs0[k - 1]);
            }
        }
        cspace::lemma_ready_seq_frame(rv0, tv0, rvf, tvf, l);
    }

    // ready_wf assembly.
    assert(rvf.heads.dom() == Set::new(|i: int| 0 <= i < NUM_PRIOS as int));
    assert(rvf.tails.dom() == Set::new(|i: int| 0 <= i < NUM_PRIOS as int));
    assert forall|lv: int| #![trigger rvf.heads[lv]] 0 <= lv < NUM_PRIOS as int implies
        (rvf.heads[lv] is None <==> rvf.tails[lv] is None) by {
        if lv == level {
            let rk = rs0.remove(k);
            assert(cspace::ready_chain(rvf, tvf, level, rk));
            if rk.len() == 0 {
                assert(rvf.heads[level] is None && rvf.tails[level] is None);
            } else {
                assert(rvf.heads[level] is Some && rvf.tails[level] is Some);
            }
        } else {
            assert(rvf.heads[lv] == rv0.heads[lv] && rvf.tails[lv] == rv0.tails[lv]);
        }
    }
    assert(cspace::ready_seq(rvf, tvf, level) == rs0.remove(k));
    assert forall|lv: int| #![trigger cspace::ready_seq(rvf, tvf, lv)] 0 <= lv < NUM_PRIOS as int
        implies cspace::ready_chain(rvf, tvf, lv, cspace::ready_seq(rvf, tvf, lv)) by {
        if lv != level {
            assert(cspace::ready_seq(rvf, tvf, lv) == cspace::ready_seq(rv0, tv0, lv));
        }
    }

    // bitmap coherence: clear-bit lemma when the level empties, inline when it stays
    // non-empty (the bitmap is then unchanged and rv0 coherence transfers directly).
    if rs0.remove(k).len() == 0 {
        assert(cspace::ready_seq(rvf, tvf, level).len() == 0);
        lemma_ready_coherent_after_clear(rv0, tv0, rvf, tvf, level);
    } else {
        assert(rvf.bitmap == rv0.bitmap);
        assert forall|l: int| 0 <= l < NUM_PRIOS implies
            ((rvf.bitmap & (1u32 << (l as u32))) != 0
                <==> (#[trigger] cspace::ready_seq(rvf, tvf, l)).len() > 0) by {
            if l == level {
                assert(l as u32 == level as u32);
                assert(cspace::ready_seq(rvf, tvf, l) == rs0.remove(k));
                assert(cspace::ready_seq(rv0, tv0, level).len() > 0);
                assert((rv0.bitmap & (1u32 << (level as u32)) != 0)
                    == (cspace::ready_seq(rv0, tv0, level).len() > 0));
            } else {
                assert(cspace::ready_seq(rvf, tvf, l) == cspace::ready_seq(rv0, tv0, l));
                assert((rv0.bitmap & (1u32 << (l as u32)) != 0)
                    == (cspace::ready_seq(rv0, tv0, l).len() > 0));
            }
        }
    }
    assert(cspace::ready_bitmap_coherent(rvf, tvf));
    assert(cspace::ready_wf(rvf, tvf));
}

// Append `t` to the tail of its priority level's ready list and set the level's bit —
// the verified core of `Store::make_runnable` (`wait`'s tail-append, minus the census:
// a ready thread holds no object ref). `t` must be off the ready queue (`state !=
// Runnable`), so the push preserves `no_duplicates`. After: `t` is `Runnable` at the tail.
#[verifier::spinoff_prover]
#[verifier::rlimit(40)]
pub fn ready_enqueue<S: Store>(store: &mut S, t: ObjId)
    requires
        old(store).tcb_view().dom().contains(t),
        (old(store).tcb_view()[t].priority as int) < NUM_PRIOS,
        old(store).tcb_view()[t].state != ThreadState::Runnable,
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).refs_view() == old(store).refs_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        cspace::ready_wf(final(store).ready_view(), final(store).tcb_view()),
        cspace::ready_complete(final(store).ready_view(), final(store).tcb_view()),
        // tcb_view changes only at `t` (now Runnable, tail) and the old level-tail (its
        // qnext now points at `t`); `t`'s non-state/qnext fields are framed.
        final(store).tcb_view()[t] == (cspace::TcbView {
            state: ThreadState::Runnable,
            qnext: None,
            ..old(store).tcb_view()[t]
        }),
        forall|x: ObjId| #![trigger final(store).tcb_view()[x]]
            x != t && old(store).ready_view().tails[old(store).tcb_view()[t].priority as int] != Some(x)
            ==> final(store).tcb_view()[x] == old(store).tcb_view()[x],
        cspace::ready_seq(final(store).ready_view(), final(store).tcb_view(),
            old(store).tcb_view()[t].priority as int)
            == cspace::ready_seq(old(store).ready_view(), old(store).tcb_view(),
                old(store).tcb_view()[t].priority as int).push(t),
{
    let ghost rv0 = old(store).ready_view();
    let ghost tv0 = old(store).tcb_view();
    let ghost level = tv0[t].priority as int;
    let ghost rs0 = cspace::ready_seq(rv0, tv0, level);
    proof {
        assert(cspace::ready_chain(rv0, tv0, level, rs0));
        // `t` is off level's chain: every charted node is Runnable, `t` is not.
        assert forall|i: int| 0 <= i < rs0.len() implies #[trigger] rs0[i] != t by {
            assert(tv0[rs0[i]].state == ThreadState::Runnable);
        }
    }

    let prio = store.tcb_priority(t) as usize;
    store.set_tcb_state(t, ThreadState::Runnable);
    store.set_tcb_qnext(t, None);
    let ghost old_tail = rv0.tails[level];
    match store.ready_tail(prio) {
        None => store.set_ready_head(prio, Some(t)),
        Some(tail) => store.set_tcb_qnext(tail, Some(t)),
    }
    store.set_ready_tail(prio, Some(t));
    let bm = store.ready_bitmap();
    store.set_ready_bitmap(bm | (1u32 << (prio as u32)));

    proof {
        let rvf = store.ready_view();
        let tvf = store.tcb_view();
        let pws = rs0.push(t);

        // ── the pushed chain at `level` ──
        assert(pws.no_duplicates()) by {
            assert forall|i: int, j: int|
                0 <= i < pws.len() && 0 <= j < pws.len() && i != j implies pws[i] != pws[j] by {
                if i < rs0.len() && j < rs0.len() {
                } else if i < rs0.len() {
                    assert(pws[i] == rs0[i] && rs0[i] != t && pws[j] == t);
                } else if j < rs0.len() {
                    assert(pws[j] == rs0[j] && rs0[j] != t && pws[i] == t);
                }
            }
        }
        assert(cspace::ready_chain(rvf, tvf, level, pws)) by {
            assert forall|i: int| 0 <= i < pws.len() implies #[trigger] tvf.dom().contains(pws[i]) by {
                if i < rs0.len() { assert(pws[i] == rs0[i]); } else { assert(pws[i] == t); }
            }
            // head / tail
            if rs0.len() == 0 {
                assert(rvf.heads[level] == Some(t));
                assert(rvf.tails[level] == Some(t));
            } else {
                assert(old_tail == Some(rs0[rs0.len() - 1]));
                assert(rvf.heads[level] == Some(rs0[0]));
                assert(rvf.tails[level] == Some(t));
            }
            // qnext threading
            assert forall|i: int| 0 <= i < pws.len() implies
                tvf[pws[i]].qnext == (if i + 1 < pws.len() { Some(pws[i + 1]) } else { None::<ObjId> }) by {
                if i + 1 < rs0.len() {
                    assert(pws[i] == rs0[i] && rs0[i] != t);
                    assert(Some(rs0[i]) != old_tail);   // not the tail (i is not last)
                    assert(tv0[rs0[i]].qnext == Some(rs0[i + 1]));
                } else if i + 1 == rs0.len() {
                    // old tail: qnext retargeted to `t`.
                    assert(pws[i] == rs0[i] && pws[i + 1] == t);
                    assert(old_tail == Some(rs0[i]));
                } else {
                    assert(pws[i] == t);   // `t` itself: qnext None.
                }
            }
            // covenant: Runnable at `level`
            assert forall|i: int| 0 <= i < pws.len() implies
                tvf[pws[i]].state == ThreadState::Runnable && tvf[pws[i]].priority as int == level by {
                if i < rs0.len() {
                    assert(pws[i] == rs0[i] && rs0[i] != t);
                } else {
                    assert(pws[i] == t);
                }
            }
        }
        // ── local frame facts, then delegate the 32-level sweep to lemma_ready_push_wf ──
        assert(prio as int == level);
        assert(prio as u32 == level as u32);
        assert(tv0[t].priority as int == level);
        // the old tail (if any) is `rs0`'s last node — priority `level`.
        assert(old_tail matches Some(y) ==> tv0[y].priority as int == level) by {
            if let Some(y) = old_tail {
                assert(rs0.len() > 0 && y == rs0[rs0.len() - 1]);
            }
        }
        assert forall|x: ObjId| x != t && old_tail != Some(x) implies #[trigger] tvf[x] == tv0[x] by {}
        assert(tvf.dom() == tv0.dom());
        assert(rvf.heads.dom() =~= rv0.heads.dom()) by { assert(rv0.heads.dom().contains(level)); }
        assert(rvf.tails.dom() =~= rv0.tails.dom()) by { assert(rv0.tails.dom().contains(level)); }
        assert forall|l: int| 0 <= l < NUM_PRIOS && l != level implies
            #[trigger] rvf.heads[l] == rv0.heads[l] && rvf.tails[l] == rv0.tails[l] by {}
        assert(bm == rv0.bitmap);
        assert(rvf.bitmap == rv0.bitmap | (1u32 << (level as u32)));
        lemma_ready_push_wf(rv0, tv0, rvf, tvf, level, t, pws);
    }
}

// Pop the head of `level`'s ready list (round-robin within a level is FIFO), clearing the
// level's bit if it empties — the verified core of the scheduler's `dequeue(top)`. Total:
// `None` (a no-op) on an empty level. The popped thread stays `Runnable` and off the chain
// (the caller — `maybe_switch` — immediately sets it `Running`), so the op preserves
// `ready_wf` but *not* `ready_complete` (hence the contract carries neither completeness form).
#[verifier::spinoff_prover]
#[verifier::rlimit(60)]
pub fn ready_dequeue<S: Store>(store: &mut S, level: usize) -> (r: Option<ObjId>)
    requires
        level < NUM_PRIOS,
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).refs_view() == old(store).refs_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        cspace::ready_wf(final(store).ready_view(), final(store).tcb_view()),
        ({
            let rs0 = cspace::ready_seq(old(store).ready_view(), old(store).tcb_view(),
                level as int);
            &&& r == (if rs0.len() == 0 { None::<ObjId> } else { Some(rs0[0]) })
            &&& rs0.len() == 0 ==> {
                    &&& final(store).ready_view() == old(store).ready_view()
                    &&& final(store).tcb_view() == old(store).tcb_view()
                }
            &&& rs0.len() > 0 ==> {
                    &&& cspace::ready_seq(final(store).ready_view(), final(store).tcb_view(),
                            level as int) == rs0.drop_first()
                    &&& final(store).tcb_view()[rs0[0]].qnext is None
                    &&& final(store).tcb_view()[rs0[0]].state == old(store).tcb_view()[rs0[0]].state
                    &&& final(store).tcb_view()[rs0[0]].priority
                            == old(store).tcb_view()[rs0[0]].priority
                    &&& forall|x: ObjId| #![trigger final(store).tcb_view()[x]]
                            x != rs0[0] ==> final(store).tcb_view()[x] == old(store).tcb_view()[x]
                }
        }),
{
    let ghost rv0 = old(store).ready_view();
    let ghost tv0 = old(store).tcb_view();
    let ghost level_i = level as int;
    let ghost rs0 = cspace::ready_seq(rv0, tv0, level_i);
    proof {
        assert(cspace::ready_chain(rv0, tv0, level_i, rs0));
    }
    match store.ready_head(level) {
        None => {
            proof {
                // head None ⇒ (ready_chain head/tail agreement) the chain is empty.
                assert(rv0.heads[level_i] is None);
                assert(rs0.len() == 0);
            }
            None
        }
        Some(t) => {
            proof {
                assert(rv0.heads[level_i] == Some(t));
                assert(rs0.len() > 0);
                assert(rs0[0] == t);
            }
            let next = store.tcb_qnext(t);
            proof { assert(next == tv0[t].qnext); }
            store.set_ready_head(level, next);
            match next {
                None => {
                    store.set_ready_tail(level, None);
                    let bm = store.ready_bitmap();
                    store.set_ready_bitmap(bm & !(1u32 << (level as u32)));
                }
                Some(_) => {}
            }
            store.set_tcb_qnext(t, None);
            proof {
                let rvf = store.ready_view();
                let tvf = store.tcb_view();
                assert(rs0.remove(0) =~= rs0.drop_first());
                assert(tvf.dom() =~= tv0.dom());
                // lemma_ready_remove_chain (k = 0) final values.
                assert(rvf.heads[level_i] == tv0[t].qnext);
                if rs0.len() == 1 {
                    assert(tv0[t].qnext is None);          // chain: sole node's qnext is None
                    assert(rvf.tails[level_i] == None::<ObjId>);
                } else {
                    assert(tv0[t].qnext == Some(rs0[1]));  // chain: rs0[0].qnext == Some(rs0[1])
                    assert(rvf.tails[level_i] == rv0.tails[level_i]);
                }
                assert forall|x: ObjId| x != t implies #[trigger] tvf[x] == tv0[x] by {}
                cspace::lemma_ready_remove_chain(rv0, tv0, rvf, tvf, level_i, t, rs0, 0);
                cspace::lemma_ready_chain_unique(rvf, tvf, level_i,
                    cspace::ready_seq(rvf, tvf, level_i), rs0.remove(0));
                // per-level head/tail frame + bitmap split for lemma_ready_remove_wf.
                assert(rvf.heads.dom() =~= rv0.heads.dom());
                assert(rvf.tails.dom() =~= rv0.tails.dom());
                assert forall|l: int| 0 <= l < NUM_PRIOS && l != level_i implies
                    #[trigger] rvf.heads[l] == rv0.heads[l] && rvf.tails[l] == rv0.tails[l] by {}
                if rs0.remove(0).len() == 0 {
                    assert(rvf.bitmap == rv0.bitmap & !(1u32 << (level_i as u32)));
                } else {
                    assert(rvf.bitmap == rv0.bitmap);
                }
                lemma_ready_remove_wf(rv0, tv0, rvf, tvf, level_i, t, rs0, 0);
            }
            Some(t)
        }
    }
}

// Splice `t` out of its priority level's ready list from an arbitrary position — the verified
// core of the teardown `unqueue_ready` seam `destroy_tcb` already leans on. `ready_complete`
// guarantees a Runnable `t` is charted on `level`'s chain, so the walk finds it (the
// fall-off-end is unreachable, exactly like `timer::disarm`). The splice is census-free — a
// ready thread holds no object ref, the §1.1 simplification — so only `t`'s `qnext` (cleared)
// and its predecessor's `qnext` (re-threaded) move. `t` is left transiently Runnable-and-
// off-chain, so the op preserves `ready_wf` + `ready_complete_except(t)` (the `destroy_tcb`
// caller halts `t` to close the completeness gap — B8C-4).
#[verifier::spinoff_prover]
#[verifier::rlimit(100)]
pub fn ready_unqueue<S: Store>(store: &mut S, t: ObjId)
    requires
        old(store).tcb_view().dom().contains(t),
        old(store).tcb_view()[t].state == ThreadState::Runnable,
        (old(store).tcb_view()[t].priority as int) < NUM_PRIOS,
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).refs_view() == old(store).refs_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        cspace::ready_wf(final(store).ready_view(), final(store).tcb_view()),
        cspace::ready_complete_except(final(store).ready_view(), final(store).tcb_view(), t),
        ({
            let level = old(store).tcb_view()[t].priority as int;
            let rs0 = cspace::ready_seq(old(store).ready_view(), old(store).tcb_view(), level);
            &&& cspace::ready_seq(final(store).ready_view(), final(store).tcb_view(), level)
                    == rs0.remove(rs0.index_of(t))
            &&& final(store).tcb_view()[t].qnext is None
            &&& final(store).tcb_view()[t].state == old(store).tcb_view()[t].state
            &&& final(store).tcb_view()[t].priority == old(store).tcb_view()[t].priority
            &&& final(store).tcb_view()[t].cspace == old(store).tcb_view()[t].cspace
            &&& final(store).tcb_view()[t].aspace == old(store).tcb_view()[t].aspace
            // the "signal-shaped" frame: only level's chain nodes (t + its predecessor) moved.
            &&& forall|x: ObjId| #![trigger final(store).tcb_view()[x]]
                    final(store).tcb_view()[x] != old(store).tcb_view()[x]
                    ==> old(store).tcb_view()[x].state == ThreadState::Runnable
                        && old(store).tcb_view()[x].priority as int == level
        }),
{
    let ghost rv0 = old(store).ready_view();
    let ghost tv0 = old(store).tcb_view();
    let ghost level = tv0[t].priority as int;
    let ghost rs0 = cspace::ready_seq(rv0, tv0, level);
    let lvl = store.tcb_priority(t) as usize;
    proof {
        assert(lvl as int == level);
        assert(lvl < NUM_PRIOS);
        assert(cspace::ready_chain(rv0, tv0, level, rs0));
        // ready_complete ⇒ `t` is charted on `level`'s chain, so the walk finds it.
        assert(rs0.contains(t));
    }

    let mut cur = store.ready_head(lvl);
    let mut prev: Option<ObjId> = None;
    let ghost mut k: int = 0;

    while cur.is_some()
        invariant
            // the walk is read-only: every view pinned to entry.
            store.slot_view() == old(store).slot_view(),
            store.refs_view() == old(store).refs_view(),
            store.chan_view() == old(store).chan_view(),
            store.notif_view() == old(store).notif_view(),
            store.tcb_view() == tv0,
            store.ready_view() == rv0,
            store.timer_view() == old(store).timer_view(),
            store.timer_head_view() == old(store).timer_head_view(),
            store.cspace_view() == old(store).cspace_view(),
            rv0 == old(store).ready_view(),
            tv0 == old(store).tcb_view(),
            level == tv0[t].priority as int,
            0 <= level < NUM_PRIOS,
            lvl as int == level,
            lvl < NUM_PRIOS,
            rs0 == cspace::ready_seq(rv0, tv0, level),
            cspace::ready_wf(rv0, tv0),
            cspace::ready_complete(rv0, tv0),
            cspace::ready_chain(rv0, tv0, level, rs0),
            rs0.contains(t),
            // `cur`/`prev` track position `k` in `rs0`, no `t` seen yet.
            0 <= k <= rs0.len(),
            cur == (if k < rs0.len() { Some(rs0[k]) } else { None::<ObjId> }),
            prev == (if k == 0 { None::<ObjId> } else { Some(rs0[k - 1]) }),
            forall|i: int| 0 <= i < k ==> rs0[i] != t,
        decreases rs0.len() - k,
    {
        let c = cur.unwrap();
        assert(k < rs0.len());
        assert(c == rs0[k]);
        // `ObjId`'s exec `==` is external; compare the u64 tag (the `remove_waiter` idiom).
        if c.0 == t.0 {
            assert(t == rs0[k]);
            let ghost len = rs0.len() as int;
            assert(rs0.len() > 0);
            assert(rv0.heads[level] == Some(rs0[0]));
            assert(rv0.tails[level] == Some(rs0[len - 1]));
            // the tail names `t` iff `t` is the last element (no_duplicates).
            assert((rs0[len - 1] == t) == (k == len - 1)) by {
                if rs0[len - 1] == t { assert(rs0[len - 1] == rs0[k]); }
                if k == len - 1 { assert(rs0[len - 1] == rs0[k]); }
            }

            let next = store.tcb_qnext(t);
            assert(next == tv0[t].qnext);

            // head-vs-middle splice (the predecessor's `qnext`, or the level head, retargets
            // past `t`).
            match prev {
                None => {
                    store.set_ready_head(lvl, next);
                }
                Some(p) => {
                    proof { assert(k > 0); assert(p == rs0[k - 1]); assert(tv0.dom().contains(p)); }
                    store.set_tcb_qnext(p, next);
                }
            }
            // tail fixup: if `t` was the tail, the new tail is `prev`.
            let tail_is_t = match store.ready_tail(lvl) {
                Some(tl) => tl.0 == t.0,
                None => false,
            };
            if tail_is_t {
                store.set_ready_tail(lvl, prev);
            }
            store.set_tcb_qnext(t, None);
            // clear the level's bit if the splice emptied it.
            let h = store.ready_head(lvl);
            if h.is_none() {
                let bm = store.ready_bitmap();
                store.set_ready_bitmap(bm & !(1u32 << (lvl as u32)));
            }

            proof {
                let rvf = store.ready_view();
                let tvf = store.tcb_view();
                // `t == rs0[k]` ⇒ `index_of(t) == k` (no_duplicates).
                assert(rs0.index_of(t) == k) by {
                    let idx = rs0.index_of(t);
                    assert(0 <= idx < rs0.len() && rs0[idx] == t);
                }
                assert(tvf.dom() =~= tv0.dom());
                // ── lemma_ready_remove_chain final values (head/tail/qnext, keyed on k) ──
                if k == 0 {
                    assert(rvf.heads[level] == tv0[t].qnext);
                } else {
                    assert(rvf.heads[level] == rv0.heads[level]);
                    assert(tvf[rs0[k - 1]].qnext == tv0[t].qnext);
                    assert(tvf[rs0[k - 1]].state == tv0[rs0[k - 1]].state);
                    assert(tvf[rs0[k - 1]].priority == tv0[rs0[k - 1]].priority);
                }
                if k == len - 1 {
                    assert(rvf.tails[level]
                        == (if k == 0 { None::<ObjId> } else { Some(rs0[k - 1]) }));
                } else {
                    assert(rvf.tails[level] == rv0.tails[level]);
                }
                assert forall|j: ObjId| j != t && (k == 0 || j != rs0[k - 1])
                    implies #[trigger] tvf[j] == tv0[j] by {}
                cspace::lemma_ready_remove_chain(rv0, tv0, rvf, tvf, level, t, rs0, k);
                cspace::lemma_ready_chain_unique(rvf, tvf, level,
                    cspace::ready_seq(rvf, tvf, level), rs0.remove(k));
                // ── bitmap split: cleared iff the level emptied ──
                assert(rvf.heads[level] is None <==> rs0.remove(k).len() == 0) by {
                    assert(cspace::ready_chain(rvf, tvf, level, rs0.remove(k)));
                }
                assert(lvl as u32 == level as u32);
                if rs0.remove(k).len() == 0 {
                    assert(rvf.heads[level] is None);
                    assert(rvf.bitmap == rv0.bitmap & !(1u32 << (level as u32)));
                } else {
                    assert(rvf.heads[level] is Some);
                    assert(rvf.bitmap == rv0.bitmap);
                }
                // ── per-level head/tail frame, then the 32-level wf sweep ──
                assert(rvf.heads.dom() =~= rv0.heads.dom()) by {
                    assert(rv0.heads.dom().contains(level));
                }
                assert(rvf.tails.dom() =~= rv0.tails.dom()) by {
                    assert(rv0.tails.dom().contains(level));
                }
                assert forall|l: int| 0 <= l < NUM_PRIOS && l != level implies
                    #[trigger] rvf.heads[l] == rv0.heads[l] && rvf.tails[l] == rv0.tails[l] by {}
                lemma_ready_remove_wf(rv0, tv0, rvf, tvf, level, t, rs0, k);

                // ── ready_complete_except(t): every surviving Runnable thread stays charted ──
                rs0.remove_ensures(k);
                assert(cspace::ready_seq(rvf, tvf, level) == rs0.remove(k));
                assert forall|x: ObjId| #[trigger] tvf.dom().contains(x)
                    && tvf[x].state == ThreadState::Runnable && x != t
                    implies (tvf[x].priority as int) < NUM_PRIOS
                        && cspace::ready_seq(rvf, tvf, tvf[x].priority as int).contains(x) by {
                    // `tv0[x]` is Runnable in both cases: unchanged ⇒ from `tvf[x] == tv0[x]`;
                    // the changed `x != t` is `t`'s predecessor `rs0[k-1]`, on the chain.
                    if tvf[x] != tv0[x] {
                        assert(k > 0 && x == rs0[k - 1]);
                        assert(tv0[rs0[k - 1]].state == ThreadState::Runnable);
                    }
                    assert(tv0.dom().contains(x));
                    assert(tv0[x].state == ThreadState::Runnable);
                    let px = tv0[x].priority as int;
                    // only `qnext` ever moved, so `x`'s priority/state survive the splice.
                    assert(tvf[x].priority as int == px);
                    // rv0-`ready_complete` charts `x` at `px`.
                    assert(px < NUM_PRIOS && cspace::ready_seq(rv0, tv0, px).contains(x));
                    if px != level {
                        assert(cspace::ready_seq(rvf, tvf, px) == cspace::ready_seq(rv0, tv0, px));
                    } else {
                        // `x ∈ rs0`, `x != t == rs0[k]` ⇒ `x ∈ rs0.remove(k)`.
                        assert(rs0.contains(x));
                        let j = rs0.index_of(x);
                        assert(0 <= j < rs0.len() && rs0[j] == x);
                        assert(j != k) by {
                            if j == k {
                                assert(rs0[j] == rs0[k]);
                                assert(rs0[k] == t);
                            }
                        }
                        let widx = if j < k { j } else { j - 1 };
                        assert(0 <= widx < rs0.remove(k).len());
                        assert(rs0.remove(k)[widx] == x);
                        assert(rs0.remove(k).contains(x));
                    }
                    assert(cspace::ready_seq(rvf, tvf, px).contains(x));
                }

                // ── signal-shaped frame: only `t` and its predecessor (both Runnable at
                //    `level` in `tv0`) moved ──
                assert forall|x: ObjId| #[trigger] tvf[x] != tv0[x] implies
                    tv0[x].state == ThreadState::Runnable && tv0[x].priority as int == level by {
                    if x != t {
                        assert(k > 0 && x == rs0[k - 1]);
                        assert(0 <= k - 1 < rs0.len());
                        assert(tv0[rs0[k - 1]].state == ThreadState::Runnable
                            && tv0[rs0[k - 1]].priority as int == level);
                    }
                }
                assert(tvf[t].qnext is None);
            }
            return;
        }
        prev = cur;
        cur = store.tcb_qnext(c);
        proof {
            k = k + 1;
        }
    }
    // Unreachable: `ready_complete` put `t` on the chain, so the walk cannot fall off the end.
    proof {
        assert(k == rs0.len());
        assert(!rs0.contains(t)) by {
            assert forall|i: int| 0 <= i < rs0.len() implies rs0[i] != t by {}
        }
        assert(false);
    }
}

} // verus!
