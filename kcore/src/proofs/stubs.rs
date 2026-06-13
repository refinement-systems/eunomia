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

#![cfg(kani)]

use crate::channel::Channel;
use crate::cspace::CSpaceObj;
use crate::env::Env;
use crate::thread::Tcb;

pub unsafe fn no_destroy_cspace<E: Env>(_cs: *mut CSpaceObj, _env: &mut E) {}
pub unsafe fn no_destroy_channel<E: Env>(_ch: *mut Channel, _env: &mut E) {}
pub unsafe fn no_destroy_tcb<E: Env>(_t: *mut Tcb, _env: &mut E) {}
