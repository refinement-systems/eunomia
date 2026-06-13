//! Exhaustive CDT op-sequence replay — the "mini-TLC" (host-side, **not Kani**).
//!
//! This brute-forces *every* sequence of CDT operations
//! (`derive` / `slot_move` / `delete` / `revoke`) up to a bounded length over
//! the `BarePool`, and asserts the CapRevocation invariants after **every**
//! step:
//!
//! - `cdt_wf` — the executable `TypeOK`, which subsumes TLA `LiveParent` (an
//!   occupied slot's non-null parent must be occupied) and `DeadNowhere`
//!   within the pool (an empty slot is fully detached);
//! - the refcount census — `RefCountSound`: the single notification's
//!   `hdr.refs` equals the number of occupied slots (every occupied slot holds
//!   one cap to it).
//!
//! ## Why this exists (finding DN-12, `doc/results/10_kani-findings-8.md`)
//!
//! CBMC OOMs on a *nondeterministic multi-step* harness that mixes the
//! destructive ops: `delete`/`revoke` dispatch through `obj_unref`'s
//! symbolic-discriminant `match`, and a possible delete at each of K steps
//! unrolls a formula the solver runs out of memory on (the 4-op alphabet OOMs
//! at K = 2). So the per-PR Kani suite checks the destructive ops only
//! singly/concretely — `revoke` on **one** concrete 5-cap tree
//! (`teardown::check_revoke`), `delete` inductively over a nondet shape at
//! **one** step (`transition::check_delete_step`). The *composition* — does the
//! whole CDT algebra preserve its invariants over many interleaved ops,
//! `revoke` included, over **all** reachable shapes? — is what TLC covers for
//! the abstract model but CBMC cannot cover for the code.
//!
//! This test fills that exact gap with a different tool: concrete exhaustive
//! enumeration in plain Rust, which has none of CBMC's blow-up. It is the
//! committed, scalable form of the one-off length-2 replay DN-12 describes, and
//! it is the only place `revoke` is exercised over arbitrary reachable trees
//! (review `doc/results/14_kani-review-2.md`, critique 2 + recommendation 1).
//!
//! ## Cost — HEAVY; run sparingly
//!
//! The sequence count is `radix ^ depth`, where `radix = 2·P² + 2·P` for a
//! `P`-slot pool. At `P = POOL_SLOTS = 4`: depth 3 ≈ 64 k sequences (< 1 s),
//! depth 4 ≈ 2.6 M (seconds), depth 5 ≈ 100 M (minutes–tens of minutes),
//! depth 6 ≈ 4 B (do not). It is therefore `#[ignore]`d — it does **not** run
//! in the normal `cargo test` suite or in CI — and is invoked explicitly via
//! `scripts/deep-verify.sh`. Depth is the `EXHAUSTIVE_DEPTH` env var
//! (default 3 when run directly; the script raises it).

#![cfg(test)]

use super::bounds::POOL_SLOTS;
use super::ghost::GhostEnv;
use super::wf::cdt_wf;
use super::world::BarePool;
use crate::cspace::{self, Cap, CapKind, Rights};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Derive,
    Move,
    Delete,
    Revoke,
}

#[derive(Clone, Copy, Debug)]
struct Step {
    op: Op,
    a: usize,
    b: usize,
}

/// Every per-step choice: the two binary ops over all `(a, b)` pairs, the two
/// unary ops over all `a`. Guards (mirrored from the TLA action preconditions)
/// turn ill-typed picks into no-ops at apply time, exactly as a guarded TLA
/// action that is simply not enabled. `radix = 2·P² + 2·P`.
fn step_alphabet() -> Vec<Step> {
    let p = POOL_SLOTS;
    let mut v = Vec::with_capacity(2 * p * p + 2 * p);
    for a in 0..p {
        for b in 0..p {
            v.push(Step { op: Op::Derive, a, b });
            v.push(Step { op: Op::Move, a, b });
        }
        v.push(Step { op: Op::Delete, a, b: 0 });
        v.push(Step { op: Op::Revoke, a, b: 0 });
    }
    v
}

#[derive(Default)]
struct Counts {
    derive: u64,
    mv: u64,
    delete: u64,
    revoke: u64, // counted only when revoke actually had descendants to delete
}

unsafe fn occupied(pool: &mut BarePool) -> u32 {
    let mut c = 0;
    for i in 0..POOL_SLOTS {
        if !(*pool.slot(i)).cap.is_empty() {
            c += 1;
        }
    }
    c
}

/// TLA `Init`: a single root cap (slot 0) holding the pool's notification.
unsafe fn init(pool: &mut BarePool) {
    let n = pool.notif_ptr();
    (*pool.slot(0)).cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
    (*n).hdr.refs = 1;
}

/// Apply one step if its guard holds (else a no-op), recording whether it
/// really executed so the run can prove it was not all no-ops.
unsafe fn apply(pool: &mut BarePool, env: &mut GhostEnv, s: Step, counts: &mut Counts) {
    let (a, b) = (s.a, s.b);
    let occ = |i: usize, pool: &mut BarePool| !(*pool.slot(i)).cap.is_empty();
    match s.op {
        Op::Derive => {
            if a != b && occ(a, pool) && !occ(b, pool) {
                // derive returns Err for an occupied dst / Untyped src; our
                // caps are notifications and dst is empty, so it succeeds.
                if cspace::derive(pool.slot(a), pool.slot(b), Rights::ALL.0).is_ok() {
                    counts.derive += 1;
                }
            }
        }
        Op::Move => {
            if a != b && occ(a, pool) && !occ(b, pool) {
                cspace::slot_move(pool.slot(a), pool.slot(b));
                counts.mv += 1;
            }
        }
        Op::Delete => {
            if occ(a, pool) {
                cspace::delete(pool.slot(a), env);
                counts.delete += 1;
            }
        }
        Op::Revoke => {
            if occ(a, pool) {
                let had_children = !(*pool.slot(a)).first_child.is_null();
                cspace::revoke(pool.slot(a), env);
                if had_children {
                    counts.revoke += 1;
                }
            }
        }
    }
}

/// `cdt_wf` (⊇ LiveParent + DeadNowhere-in-pool) and the refcount census
/// (RefCountSound), the CapRevocation safety invariants, after a step.
unsafe fn check(pool: &mut BarePool, seq: &[Step]) {
    let slots = pool.slot_ptrs();
    assert!(cdt_wf(&slots), "cdt_wf violated after {seq:?}");
    let refs = (*pool.notif_ptr()).hdr.refs;
    let occ = occupied(pool);
    assert!(refs == occ, "census violated after {seq:?}: refs={refs} occupied={occ}");
}

/// Mixed-radix odometer: increment `idx` (each digit in `0..radix`); return
/// `false` when it rolls over (enumeration complete).
fn advance(idx: &mut [usize], radix: usize) -> bool {
    for d in idx.iter_mut() {
        *d += 1;
        if *d < radix {
            return true;
        }
        *d = 0;
    }
    false
}

#[test]
#[ignore = "HEAVY exhaustive CDT replay — run via scripts/deep-verify.sh; see module docs"]
fn exhaustive_cdt_replay() {
    let depth: usize = std::env::var("EXHAUSTIVE_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&d| d >= 1)
        .unwrap_or(3);

    let alphabet = step_alphabet();
    let radix = alphabet.len();
    let total: u128 = (radix as u128).pow(depth as u32);
    println!(
        "exhaustive CDT replay: P={POOL_SLOTS} depth={depth} radix={radix} → {total} sequences \
         (ops: derive/move/delete/revoke; invariants: cdt_wf + refcount census)"
    );
    let start = std::time::Instant::now();

    let mut counts = Counts::default();
    let mut idx = vec![0usize; depth];
    let mut seq = vec![alphabet[0]; depth];
    let mut done: u128 = 0;

    loop {
        for d in 0..depth {
            seq[d] = alphabet[idx[d]];
        }

        let mut pool = BarePool::new();
        let mut env = GhostEnv::new();
        unsafe {
            init(&mut pool);
            check(&mut pool, &seq[..0]); // Init is itself wf + sound
            for step_i in 0..depth {
                apply(&mut pool, &mut env, seq[step_i], &mut counts);
                check(&mut pool, &seq[..=step_i]);
            }
        }

        done += 1;
        if done % 2_000_000 == 0 {
            println!("  {done}/{total} sequences ({:.1?})", start.elapsed());
        }
        if !advance(&mut idx, radix) {
            break;
        }
    }

    println!(
        "OK: {done} sequences in {:.1?}; real ops applied — derive={} move={} delete={} revoke={}",
        start.elapsed(),
        counts.derive,
        counts.mv,
        counts.delete,
        counts.revoke,
    );

    // Non-vacuity (the `cover!` analog): the enumeration must have exercised
    // each op past its guard, or a guard bug silently turned everything into
    // no-ops. `revoke` with descendants needs ≥2 steps (derive then revoke).
    assert!(counts.derive > 0, "no real derive executed — guards may be vacuous");
    assert!(counts.mv > 0, "no real move executed — guards may be vacuous");
    assert!(counts.delete > 0, "no real delete executed — guards may be vacuous");
    if depth >= 2 {
        assert!(counts.revoke > 0, "no revoke-with-descendants executed — guards may be vacuous");
    }
}
