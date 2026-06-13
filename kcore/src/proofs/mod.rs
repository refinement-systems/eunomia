//! Proof infrastructure and Kani harnesses (plan §4.1). Split into:
//! - [`bounds`]  — the TLC-scale verification bounds, in one place;
//! - [`ghost`]   — `GhostEnv`, the recording [`crate::env::Env`];
//! - [`world`]   — harness-owned object pools with real provenance;
//! - [`wf`]      — `cdt_wf` / `chan_wf` / the refcount census.
//!
//! The well-formedness predicates double as `cargo test` oracles (the unit
//! tests below), and the Kani harnesses (added per plan phases 3–4) assert
//! them after each operation.

// Infrastructure is shared by harnesses that land across several PRs; not
// every helper is exercised until its harness exists.
#![allow(dead_code)]

pub mod bounds;
#[cfg(kani)]
pub mod channel;
#[cfg(kani)]
pub mod cdt;
pub mod ghost;
#[cfg(kani)]
pub mod notification;
#[cfg(kani)]
pub mod teardown;
#[cfg(kani)]
pub mod thread;
#[cfg(kani)]
pub mod transition;
#[cfg(kani)]
pub mod untyped;
pub mod wf;
pub mod world;

#[cfg(test)]
mod tests {
    use super::ghost::{GhostEnv, GhostEvent};
    use super::wf::{cdt_wf, chan_wf, recompute_refs, refcount_sound};
    use super::world::World;
    use crate::cspace::{self, Cap, CapKind, Rights};
    use crate::notification::NotifObj;

    /// A non-empty notification cap (handy filler for CDT-shape tests).
    fn notif_cap(n: *mut NotifObj) -> Cap {
        Cap { kind: CapKind::Notification(n), rights: Rights::ALL }
    }

    #[test]
    fn layout_matches_inline_arrays() {
        let mut w = World::new();
        w.assert_layout();
    }

    #[test]
    fn empty_world_is_wf_and_sound() {
        let mut w = World::new();
        let slots = w.collect_slots();
        unsafe {
            assert!(cdt_wf(&slots));
            assert!(chan_wf(w.channel()));
            assert!(refcount_sound(&mut w));
        }
    }

    #[test]
    fn built_cdt_chain_is_wf() {
        // cs0.slot0 (root) -> slot1 -> slot2, all notif caps to notif 0.
        let mut w = World::new();
        let n = w.notif(0);
        unsafe {
            let s0 = w.cspace_slot(0, 0);
            let s1 = w.cspace_slot(0, 1);
            let s2 = w.cspace_slot(0, 2);
            (*s0).cap = notif_cap(n);
            (*s1).cap = notif_cap(n);
            (*s2).cap = notif_cap(n);
            cspace::cdt_insert_child(s0, s1);
            cspace::cdt_insert_child(s1, s2);
            recompute_refs(&mut w);

            let slots = w.collect_slots();
            assert!(cdt_wf(&slots));
            assert!(refcount_sound(&mut w));
            // notif 0 has three caps to it.
            assert_eq!((*n).hdr.refs, 3);
            // s1 is s0's only child; s2 is s1's child.
            assert_eq!((*s0).first_child, s1);
            assert_eq!((*s1).first_child, s2);
        }
    }

    #[test]
    fn broken_sibling_link_fails_wf() {
        let mut w = World::new();
        let n = w.notif(0);
        unsafe {
            let s0 = w.cspace_slot(0, 0);
            let s1 = w.cspace_slot(0, 1);
            (*s0).cap = notif_cap(n);
            (*s1).cap = notif_cap(n);
            cspace::cdt_insert_child(s0, s1);
            // Corrupt: claim s1 has a prev sibling that doesn't point back.
            (*s1).prev_sib = s0;
            let slots = w.collect_slots();
            assert!(!cdt_wf(&slots), "back-link inconsistency must fail cdt_wf");
        }
    }

    #[test]
    fn corrupt_refcount_fails_soundness() {
        let mut w = World::new();
        let n = w.notif(0);
        unsafe {
            let s0 = w.cspace_slot(0, 0);
            (*s0).cap = notif_cap(n);
            recompute_refs(&mut w);
            assert!(refcount_sound(&mut w));
            // Inflate the refcount; the census no longer matches.
            (*n).hdr.refs += 1;
            assert!(!refcount_sound(&mut w));
        }
    }

    #[test]
    fn out_of_window_ring_cap_fails_chan_wf() {
        let mut w = World::new();
        let n = w.notif(0);
        unsafe {
            // Empty rings (count 0): every ring slot is "outside the window",
            // so any cap parked there breaks chan_wf.
            let r = w.ring_cap(0, 0, 0);
            (*r).cap = notif_cap(n);
            assert!(!chan_wf(w.channel()));
        }
    }

    #[test]
    fn delete_reparents_children_and_keeps_wf() {
        // root s0 -> s1 -> {s2}; delete s1; s2 re-parents up to s0.
        let mut w = World::new();
        let n = w.notif(0);
        unsafe {
            let s0 = w.cspace_slot(0, 0);
            let s1 = w.cspace_slot(0, 1);
            let s2 = w.cspace_slot(0, 2);
            (*s0).cap = notif_cap(n);
            (*s1).cap = notif_cap(n);
            (*s2).cap = notif_cap(n);
            cspace::cdt_insert_child(s0, s1);
            cspace::cdt_insert_child(s1, s2);
            recompute_refs(&mut w);

            cspace::delete(s1, &mut w.env);

            let slots = w.collect_slots();
            assert!(cdt_wf(&slots));
            assert!((*s1).cap.is_empty());
            assert_eq!((*s2).parent, s0, "s2 re-parents one level up");
            assert!(refcount_sound(&mut w));
            assert_eq!((*n).hdr.refs, 2, "one notif cap deleted");
        }
    }

    #[test]
    fn ghost_env_records_make_runnable_on_signal() {
        let mut env = GhostEnv::new();
        let mut w = World::new();
        // Block tcb0 on notif0, then signal: the wake is recorded.
        unsafe {
            let n = w.notif(0);
            let t = w.tcb(0);
            assert!(crate::notification::wait(n, t).is_none());
            crate::notification::signal(n, 0b1, &mut env);
            assert_eq!(env.count(GhostEvent::MakeRunnable(t)), 1);
            assert_eq!((*t).frame.x[0], 0b1, "waiter received the word");
        }
    }
}
