//! Exhaustive op-sequence replays — the "mini-TLC" (host-side, **not Kani**).
//!
//! Two `#[ignore]`d tests brute-force *every* sequence of CDT operations
//! (`derive` / `slot_move` / `delete` / `revoke`) up to a bounded length and
//! assert the CapRevocation invariants after **every** step:
//!
//! - [`exhaustive_cdt_replay`] — over the flat `BarePool` (cspace-resident caps
//!   only): the composition of the CDT algebra over all reachable *tree*
//!   shapes.
//! - [`exhaustive_cross_home_replay`] — over a `World`, parking derived caps in
//!   a **channel ring slot** and a **TCB binding slot** as well as cspace
//!   slots: the §2.2 / §3.4 guarantee that *revocation sees through queues and
//!   TCB binding slots* — over all reachable shapes, not the single concrete
//!   tree of `teardown::check_revoke`.
//!
//! Invariants asserted each step: `cdt_wf` (executable `TypeOK`, ⊇ `LiveParent`
//! and `DeadNowhere`-in-universe), the refcount census (`RefCountSound`), and —
//! for the World replay — `chan_wf`.
//!
//! ## Why these exist (finding DN-12, `doc/results/10_kani-findings-8.md`)
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
//! `revoke` included, over **all** reachable shapes and across all three
//! CDT-visible homes? — is what TLC covers for the abstract model but CBMC
//! cannot cover for the code.
//!
//! These tests fill that gap with a different tool: concrete exhaustive
//! enumeration in plain Rust, which has none of CBMC's blow-up (review
//! `doc/results/14_kani-review-2.md`, critique 2 + recommendations 1 & "more").
//!
//! ## Cost — HEAVY; run sparingly
//!
//! Sequence count is `radix ^ depth`, `radix = 2·T² + 2·T` for `T` target
//! slots. BarePool `T = POOL_SLOTS = 4` (radix 40): depth 3 ≈ 64 k (< 1 s),
//! depth 5 ≈ 100 M (~15 s release). Cross-home `T = 5` (radix 60): depth 3
//! ≈ 216 k, depth 4 ≈ 13 M. Both are `#[ignore]`d — they do **not** run in the
//! normal `cargo test` suite. CI runs them at a cheap depth (`host-tests`); the
//! deep depths run via `scripts/deep-verify.sh` / the scheduled kani-deep job.
//! Depths: `EXHAUSTIVE_DEPTH` (BarePool, default 3) and `CROSS_HOME_DEPTH`
//! (World, default 3).

#![cfg(test)]

use super::bounds::POOL_SLOTS;
use super::ghost::GhostEnv;
use super::wf::{cdt_wf, chan_wf, refcount_sound};
use super::world::{BarePool, World};
use crate::cspace::{self, Cap, CapKind, CapSlot, Rights};
use crate::thread::BIND_EXIT;
use std::time::Instant;

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

/// Every per-step choice over `n` target slots: the two binary ops over all
/// `(a, b)` pairs, the two unary ops over all `a`. Guards (mirrored from the
/// TLA action preconditions) turn an ill-typed pick into a no-op at apply time,
/// exactly as a guarded TLA action that is simply not enabled. `radix = 2·n² + 2·n`.
fn step_alphabet(n: usize) -> Vec<Step> {
    let mut v = Vec::with_capacity(2 * n * n + 2 * n);
    for a in 0..n {
        for b in 0..n {
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

/// Apply one step to `targets[a]` / `targets[b]` if its guard holds (else a
/// no-op), recording whether it really executed — container-agnostic, so both
/// replays share it. `targets` are raw `CapSlot` pointers into whatever pool
/// the caller built; the op machinery is identical regardless of which home
/// (cspace / channel ring / TCB bind) each slot physically lives in.
unsafe fn apply_over(targets: &[*mut CapSlot], env: &mut GhostEnv, s: Step, counts: &mut Counts) {
    let occ = |p: *mut CapSlot| !(*p).cap.is_empty();
    let (sa, sb) = (targets[s.a], targets[s.b]);
    match s.op {
        Op::Derive => {
            if s.a != s.b && occ(sa) && !occ(sb) {
                // derive returns Err for an occupied dst / Untyped src; our
                // caps are notifications and dst is empty, so it succeeds.
                if cspace::derive(sa, sb, Rights::ALL.0).is_ok() {
                    counts.derive += 1;
                }
            }
        }
        Op::Move => {
            if s.a != s.b && occ(sa) && !occ(sb) {
                cspace::slot_move(sa, sb);
                counts.mv += 1;
            }
        }
        Op::Delete => {
            if occ(sa) {
                cspace::delete(sa, env);
                counts.delete += 1;
            }
        }
        Op::Revoke => {
            if occ(sa) {
                let had_children = !(*sa).first_child.is_null();
                cspace::revoke(sa, env);
                if had_children {
                    counts.revoke += 1;
                }
            }
        }
    }
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

fn read_depth(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&d| d >= 1)
        .unwrap_or(default)
}

/// Drive the odometer over all `radix^depth` sequences, invoking `per_seq` on
/// each. `per_seq` builds a fresh container, replays the sequence, and asserts
/// the invariants after every step. Returns the sequence count.
fn enumerate(label: &str, n_targets: usize, depth: usize, mut per_seq: impl FnMut(&[Step])) -> u128 {
    let alphabet = step_alphabet(n_targets);
    let radix = alphabet.len();
    let total: u128 = (radix as u128).pow(depth as u32);
    println!("{label}: targets={n_targets} depth={depth} radix={radix} → {total} sequences");
    let start = Instant::now();
    let mut idx = vec![0usize; depth];
    let mut seq = vec![alphabet[0]; depth];
    let mut done: u128 = 0;
    loop {
        for d in 0..depth {
            seq[d] = alphabet[idx[d]];
        }
        per_seq(&seq);
        done += 1;
        if done % 2_000_000 == 0 {
            println!("  {done}/{total} ({:.1?})", start.elapsed());
        }
        if !advance(&mut idx, radix) {
            break;
        }
    }
    println!("OK: {done} sequences in {:.1?}", start.elapsed());
    done
}

/// Non-vacuity (the `cover!` analog): the enumeration must have exercised each
/// op past its guard, or a guard bug silently made everything a no-op. A
/// `revoke`-with-descendants needs ≥ 2 steps (derive then revoke).
fn assert_non_vacuous(c: &Counts, depth: usize) {
    assert!(c.derive > 0, "no real derive executed — guards may be vacuous");
    assert!(c.mv > 0, "no real move executed — guards may be vacuous");
    assert!(c.delete > 0, "no real delete executed — guards may be vacuous");
    if depth >= 2 {
        assert!(c.revoke > 0, "no revoke-with-descendants executed — guards may be vacuous");
    }
    println!(
        "real ops applied — derive={} move={} delete={} revoke={}",
        c.derive, c.mv, c.delete, c.revoke
    );
}

// ── 1. BarePool: the CDT algebra over all reachable tree shapes ─────────────

#[test]
#[ignore = "HEAVY exhaustive CDT replay — run via scripts/deep-verify.sh; see module docs"]
fn exhaustive_cdt_replay() {
    let depth = read_depth("EXHAUSTIVE_DEPTH", 3);
    let mut counts = Counts::default();
    enumerate("BarePool CDT replay", POOL_SLOTS, depth, |seq| unsafe {
        let mut pool = BarePool::new();
        let mut env = GhostEnv::new();
        let targets = pool.slot_ptrs(); // [*mut CapSlot; POOL_SLOTS]
        let n = pool.notif_ptr();
        // TLA `Init`: a single root cap (slot 0) holding the pool's notification.
        (*targets[0]).cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        (*n).hdr.refs = 1;

        let check = |targets: &[*mut CapSlot], seq: &[Step]| {
            assert!(cdt_wf(targets), "cdt_wf violated after {seq:?}");
            let occupied = targets.iter().filter(|&&s| !(*s).cap.is_empty()).count() as u32;
            assert!((*n).hdr.refs == occupied, "census violated after {seq:?}");
        };
        check(&targets, &seq[..0]);
        for i in 0..seq.len() {
            apply_over(&targets, &mut env, seq[i], &mut counts);
            check(&targets, &seq[..=i]);
        }
    });
    assert_non_vacuous(&counts, depth);
}

// ── 2. World: revoke through channel-queue and TCB-bind homes ────────────────

#[test]
#[ignore = "HEAVY cross-home exhaustive replay — run via scripts/deep-verify.sh; see module docs"]
fn exhaustive_cross_home_replay() {
    let depth = read_depth("CROSS_HOME_DEPTH", 3);
    let mut counts = Counts::default();
    // Five targets spanning the three CDT-visible homes (§2.2): two cspace
    // slots, one in-flight channel ring cap slot, one TCB on-exit binding slot,
    // one slot in a second cspace. radix = 2·5² + 2·5 = 60.
    enumerate("World cross-home replay", 5, depth, |seq| unsafe {
        let mut w = World::new();
        let mut env = GhostEnv::new();
        // Full ring window so every ring cap slot is in-window — `chan_wf` then
        // holds for any occupancy (it only constrains out-of-window slots),
        // and an emptied in-window slot (revoke's null-slot rule, §3.4) is
        // still well-formed.
        (*w.channel()).head = [0, 0];
        (*w.channel()).count = [super::bounds::CHAN_DEPTH, super::bounds::CHAN_DEPTH];

        let targets: [*mut CapSlot; 5] = [
            w.cspace_slot(0, 0),
            w.cspace_slot(0, 1),
            w.ring_cap(0, 0, 0),
            w.bind_slot(0, BIND_EXIT),
            w.cspace_slot(1, 0),
        ];
        let n = w.notif(0);
        (*targets[0]).cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        (*n).hdr.refs = 1;

        let check = |w: &mut World, seq: &[Step]| {
            let slots = w.collect_slots();
            assert!(cdt_wf(&slots), "cdt_wf violated after {seq:?}");
            assert!(chan_wf(w.channel()), "chan_wf violated after {seq:?}");
            assert!(refcount_sound(w), "census violated after {seq:?}");
        };
        check(&mut w, &seq[..0]);
        for i in 0..seq.len() {
            apply_over(&targets, &mut env, seq[i], &mut counts);
            check(&mut w, &seq[..=i]);
        }
    });
    assert_non_vacuous(&counts, depth);
}
