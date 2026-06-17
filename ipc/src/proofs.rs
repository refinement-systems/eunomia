//! Kani harnesses for the pure codecs and the admission quota (plan §4.7, §5.4).
//!
//! The fixed message header (spec §3.7) — `decode` totality, accept-iff-length,
//! and the `encode`∘`decode` bijection — **migrated to Verus** in phase 7a
//! (`crate::header`, `doc/plans/3_verus-rewrite_phase7-detail.md` §7a): the
//! `check_header_*` Kani harnesses that lived here are subsumed by the unbounded
//! `lemma_decode_encode`/`lemma_encode_decode` proofs, so they are deleted (§5:
//! a property is never unguarded between tiers). The session codecs below stay on
//! Kani until phase 7b ports them.
//!
//! The §4.6 session codecs (`ConnectReq`, `GrantReply`) get the same treatment
//! (§5.4: "any new *pure* codec helper gets a Kani harness"): `decode` is total
//! and accepts exactly the well-formed encodings, and `encode`/`decode`
//! round-trips. And `Admission` — the single window-quota admission point — is
//! proven to **never over-grant**: the `granted ≤ budget` invariant holds across
//! a bounded sequence of `admit`/`release`, so `remaining()`'s `budget - granted`
//! subtraction can never underflow (the soundness the unit tests only sample).

#![cfg(kani)]

use crate::session::{Admission, ConnectReq, GrantReply, WindowGrant};

// The §4.6 session codecs. The accepted shapes are hard-coded here to match the
// private `REQ_LEN`/`GRANT_LEN`/`REFUSED_LEN` + `TAG_*` constants in `session.rs`
// (5/`0xC0` for a request; 9/`0x01` for a grant; 1/`0x00` for a refusal); the
// round-trip harnesses below pin them indirectly.

/// `ConnectReq::decode` is total over arbitrary bytes and accepts **iff** the
/// input is exactly a 5-byte request (tag `0xC0`).
#[kani::proof]
#[kani::unwind(12)]
fn check_connect_req_decode_total() {
    let bytes: [u8; 7] = kani::any(); // REQ_LEN (5) + 2: short / exact / trailing
    let len: usize = kani::any();
    kani::assume(len <= 7);
    let r = ConnectReq::decode(&bytes[..len]);
    let valid = len == 5 && bytes[0] == 0xC0;
    assert!(r.is_some() == valid);
    // Both outcomes reachable (anti-vacuity, the CI cover guard).
    kani::cover!(r.is_some());
    kani::cover!(r.is_none());
}

/// `encode`∘`decode` is the identity on `ConnectReq`.
#[kani::proof]
fn check_connect_req_roundtrip() {
    let req = ConnectReq { requested_window: kani::any() };
    assert!(ConnectReq::decode(&req.encode()) == Some(req));
}

/// `GrantReply::decode` is total over arbitrary bytes and accepts **iff** the
/// input is exactly a 9-byte grant (tag `0x01`) or a 1-byte refusal (tag `0x00`).
#[kani::proof]
#[kani::unwind(12)]
fn check_grant_reply_decode_total() {
    let bytes: [u8; 11] = kani::any(); // GRANT_LEN (9) + 2
    let len: usize = kani::any();
    kani::assume(len <= 11);
    let r = GrantReply::decode(&bytes[..len]);
    let is_grant = len == 9 && bytes[0] == 0x01;
    let is_refused = len == 1 && bytes[0] == 0x00;
    assert!(r.is_some() == (is_grant || is_refused));
    kani::cover!(r.is_some());
    kani::cover!(r.is_none());
}

/// `encode`∘`decode` is the identity on `GrantReply` (both the grant and the
/// refusal arms; `encode` returns the used prefix length).
#[kani::proof]
fn check_grant_reply_roundtrip() {
    let g: GrantReply = if kani::any() {
        GrantReply::Grant(WindowGrant { window: kani::any(), size: kani::any() })
    } else {
        GrantReply::Refused
    };
    let (b, n) = g.encode();
    assert!(GrantReply::decode(&b[..n]) == Some(g));
}

/// `Admission` never over-grants: across a bounded sequence of symbolic
/// `admit`/`release` from a fresh quota, the `granted ≤ budget` invariant is
/// preserved — so `remaining()`'s `budget - granted` never underflows (Kani's
/// overflow checks would catch it) — and `admit` honours its contract.
#[kani::proof]
#[kani::unwind(4)]
fn check_admission_never_over_grants() {
    let budget: u32 = kani::any();
    let mut adm = Admission::new(budget);
    let mut steps = 0;
    while steps < 3 {
        steps += 1;
        // No underflow here ⟺ granted ≤ budget held entering this step.
        let before = adm.remaining();
        let req: u32 = kani::any();
        let r = adm.admit(req);
        kani::cover!(r.is_ok());
        kani::cover!(r.is_err());
        match r {
            Ok(g) => {
                // Granted exactly what was asked, only because it fit.
                assert!(g.size == req);
                assert!(req <= before);
                assert!(adm.remaining() == before - req);
            }
            Err(_) => {
                // Refused only when it did not fit; the quota is untouched.
                assert!(req > before);
                assert!(adm.remaining() == before);
            }
        }
    }
    // Releasing a (possibly oversized) grant never underflows the accounting.
    adm.release(WindowGrant { window: 0, size: kani::any() });
    let _ = adm.remaining();
}
