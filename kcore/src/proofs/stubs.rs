//! No-op stubs for the recursive container teardowns (finding DN-4), shared by
//! the harnesses that drive a top-level `cspace::delete`/`revoke`
//! ([`super::teardown`], [`super::transition`]).
//!
//! When a `delete` is the *top-level* entry, CBMC reads the deleted cap's kind
//! from slot memory as a symbolic discriminant and unrolls *every* `obj_unref`
//! match arm — including the `delete↔destroy_cspace↔destroy_channel↔
//! destroy_tcb` recursion — before the solver can prune the infeasible ones,
//! which never finishes unwinding within the CI budget. Stubbing these three
//! recursion-causing teardowns to no-ops cuts the unrolling. The teardown
//! bodies themselves stay *really* proven by the direct-call harnesses
//! (`check_destroy_cspace`, `check_destroy_channel` §4.3,
//! `check_thread_teardown` §4.4), so a stubbed harness only gives up re-proving
//! those bodies a second time through the dispatch. A harness whose pool holds
//! only leaf (notification) caps stubs these purely to prune the infeasible
//! arms — the behaviour is unchanged, only the formula shrinks.
//!
//! These no-ops are not *silent*: each records a `GhostEvent` for the
//! destructor arm `obj_unref` dispatched to (review-2 rec. 3, finding part 12),
//! so a `check_delete_*` harness asserts the routing (`count(DestroyCspace(p))
//! == 1`) rather than trusting the one-line `match` arm by source inspection —
//! the same way `check_delete_frame` already witnesses its `AspaceUnmap`. The
//! hook is a proof-only `Env` method (`Env::ghost_destroy_*`, `#[cfg(kani)]`),
//! the only handle to harness state these *generic* stubs have.

#![cfg(kani)]

use crate::channel::Channel;
use crate::cspace::CSpaceObj;
use crate::env::Env;
use crate::thread::Tcb;

pub unsafe fn no_destroy_cspace<E: Env>(cs: *mut CSpaceObj, env: &mut E) {
    env.ghost_destroy_cspace(cs);
}
pub unsafe fn no_destroy_channel<E: Env>(ch: *mut Channel, env: &mut E) {
    env.ghost_destroy_channel(ch);
}
pub unsafe fn no_destroy_tcb<E: Env>(t: *mut Tcb, env: &mut E) {
    env.ghost_destroy_tcb(t);
}
