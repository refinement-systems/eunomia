// SPDX-License-Identifier: 0BSD
//! The IPC reactor (spec rev2§3.6) — the lost-wakeup core. An epoll-shaped
//! `register(source, signals, key)` / `wait() -> (key, signals)` API over a
//! notification word's **bit-groups**.
//! It **owns the "bind, poll once, then wait" discipline**, so no server reaches
//! for a notification bit, and the rev2§3.6 wait-set kernel object is a future
//! O(1) drop-in behind this same API.
//!
//! Lost-wakeup safety has two halves, both modeled by `tla/ipc_reactor` and
//! re-checked on this code by the no-lost-wakeup harness (model.rs, Shuttle + Loom):
//!   1. `register` binds the source's events to a bit and then **self-signals**
//!      that bit — the "poll once". It forces the first `wait()` to surface the
//!      source, so a message queued *before* the bind (whose edge signal went
//!      nowhere) is still polled. Without it, the send-before-bind interleaving
//!      deadlocks (the negative control).
//!   2. `wait()` blocks via `Transport::notif_wait`, whose word-check-before-
//!      block (`kcore::notification`'s `wait`) never sleeps through a signal that
//!      already arrived.
//!
//! Two kinds of source register here. [`Reactor::register`] takes a **channel**
//! and is **level-triggered**: it `bind`s the channel's events and self-signals
//! a poll-once so a message queued before the bind still surfaces.
//! [`Reactor::register_bound`] takes an **externally-bound, edge-triggered**
//! source — a thread on-exit/on-fault binding (`thread_bind`, rev2§5.1), a timer, an
//! IRQ — already wired to a caller-chosen bit; it neither binds nor self-signals
//! (a poll-once would fabricate a one-shot event), so lost-wakeup safety there
//! rests on the caller binding before the source can fire plus `wait`'s
//! word-check. The shell's spawn/reap loop is the first `register_bound`
//! consumer; storaged is the first `register` consumer.
//!
//! Generic over `Transport`: production drives `SyscallTransport`, the harnesses
//! drive `ModelTransport`. Single-threaded per process (`wait` takes `&mut`), so
//! the reactor itself holds no locks.
use core::ops::BitOr;

use crate::transport::{Chan, Event, Notif, Transport};
use vstd::prelude::*;

verus! {

// The pure core of the reactor's bit allocator (rev2§3.6), verified. The lowest
// **clear** bit of `used` is `(!used).trailing_zeros()`: a trailing *zero* of
// `!used` is a trailing *one* of `used`, so the first zero of `!used` is the
// first free bit of `used`. `None` when the 64-bit word is full.
//
// Verified to the kcore ready-queue-bitmap pattern (`kcore/src/ready.rs`'s
// `leading_zeros` bit-scan), swapping u32/leading-zeros/highest-set for
// u64/trailing-zeros/lowest-clear: the returned bit is in range and **was clear**
// (no double-allocation — the allocator never hands out a bit it already owns),
// it is the **lowest** clear bit (the lowest-first discipline this module
// documents and the proptest `register_sequence_keeps_used_coherent` exercises),
// and it refuses (`None`) **only** when every bit is taken. The proof rests on
// `axiom_u64_trailing_zeros` (the bit-at-`tz`-set / lower-bits-clear facts) plus
// `bit_vector` to bridge the `(!used >> k) & 1` form the axiom speaks to the
// `used & (1 << k)` form the allocator's `used |= 1 << bit` speaks. No new
// trusted seam: a pure bitmask over a `vstd` axiom, no interpreted primitive.
// `alloc_lowest` (which records the allocation) calls this.
// Crate-private: this adds no public API surface, only a verified internal core.
fn lowest_clear_bit(used: u64) -> (r: Option<u32>)
    ensures
        r is None ==> used == 0xFFFF_FFFF_FFFF_FFFFu64,
        r matches Some(bit) ==> {
            &&& bit < 64
            &&& used & (1u64 << (bit as u64)) == 0u64
            &&& forall|j: u64| #![trigger used & (1u64 << j)] j < bit ==> used & (1u64 << j) != 0u64
        },
{
    broadcast use vstd::std_specs::bits::axiom_u64_trailing_zeros;

    let inv = !used;
    let bit = inv.trailing_zeros();
    if bit == 64 {
        proof {
            // tz(inv) == 64 <==> inv == 0 (axiom); inv == !used (let), so !used == 0.
            assert(inv == 0u64);
            assert(!used == 0u64);
            assert(used == 0xFFFF_FFFF_FFFF_FFFFu64) by (bit_vector)
                requires
                    !used == 0u64,
            ;
        }
        None
    } else {
        proof {
            assert(bit < 64);  // from 0 <= tz <= 64 (axiom) and bit != 64
            let g: u64 = bit as u64;
            // The bit at position `bit` of `inv` is set ⇒ that bit of `used` is clear.
            assert((inv >> g) & 1u64 == 1u64);
            assert(((!used) >> g) & 1u64 == 1u64);
            assert(used & (1u64 << g) == 0u64) by (bit_vector)
                requires
                    g < 64,
                    ((!used) >> g) & 1u64 == 1u64,
            ;
            // Every lower bit of `inv` is clear ⇒ every lower bit of `used` is set.
            assert forall|j: u64| #![trigger used & (1u64 << j)] j < bit implies used & (1u64 << j)
                != 0u64 by {
                assert((inv >> j) & 1u64 == 0u64);
                assert(((!used) >> j) & 1u64 == 0u64);
                assert(used & (1u64 << j) != 0u64) by (bit_vector)
                    requires
                        j < 64,
                        ((!used) >> j) & 1u64 == 0u64,
                ;
            }
        }
        Some(bit)
    }
}

// The pure core of the reactor's `pending` drain (rev2§3.6), verified. The lowest
// **set** bit of a non-empty word is `pending.trailing_zeros()`, and `pending &
// !(1 << bit)` is that word with exactly that bit cleared. Verified to the same
// `axiom_u64_trailing_zeros` pattern as `lowest_clear_bit` (the bit-at-`tz`-set /
// lower-bits-clear facts, bridged by `bit_vector` from the `(pending >> k) & 1`
// form the axiom speaks to the `pending & (1 << k)` form the drain speaks): the
// returned bit is in range and **was set**, it is the **lowest** set bit (the
// lowest-first drain the proptest `pending_drain_is_lowest_first` exercises), and
// the returned word is the input with only that bit cleared — no other pending
// source is lost. `requires pending != 0` matches `wait`'s refill discipline (it
// drains only the non-zero word `notif_wait` returns), so the `1 << bit` shift
// never overshoots. `wait` (plain Rust, the blocking shell) calls this.
// Crate-private: a verified internal core, no public API surface.
fn drain_one(pending: u64) -> (r: (u32, u64))
    requires
        pending != 0,
    ensures
        r.0 < 64,
        pending & (1u64 << (r.0 as u64)) != 0u64,
        forall|j: u64| #![trigger pending & (1u64 << j)] j < r.0 ==> pending & (1u64 << j) == 0u64,
        r.1 == pending & !(1u64 << (r.0 as u64)),
{
    broadcast use vstd::std_specs::bits::axiom_u64_trailing_zeros;

    let bit = pending.trailing_zeros();
    proof {
        // pending != 0 ⇒ tz(pending) < 64 (axiom: tz == 64 <==> word == 0).
        assert(bit < 64);
        let g: u64 = bit as u64;
        // The bit at position `bit` is set ⇒ that bit of `pending` is set.
        assert((pending >> g) & 1u64 == 1u64);
        assert(pending & (1u64 << g) != 0u64) by (bit_vector)
            requires
                g < 64,
                (pending >> g) & 1u64 == 1u64,
        ;
        // Every lower bit of `pending` is clear (the drained bit is the lowest).
        assert forall|j: u64| #![trigger pending & (1u64 << j)] j < bit implies pending & (1u64
            << j) == 0u64 by {
            assert((pending >> j) & 1u64 == 0u64);
            assert(pending & (1u64 << j) == 0u64) by (bit_vector)
                requires
                    j < 64,
                    (pending >> j) & 1u64 == 0u64,
            ;
        }
    }
    (bit, pending & !(1u64 << (bit as u64)))
}

} // verus!
verus! {

/// The events a source can be registered for / reported ready on (rev2§3.3, rev2§3.6).
/// A set of bits; combine with `|`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signals(u8);

} // verus!
impl Signals {
    pub const READABLE: Signals = Signals(1);
    pub const WRITABLE: Signals = Signals(2);
    pub const PEER_CLOSED: Signals = Signals(4);

    pub const fn readable(self) -> bool {
        self.0 & Self::READABLE.0 != 0
    }
    pub const fn writable(self) -> bool {
        self.0 & Self::WRITABLE.0 != 0
    }
    pub const fn peer_closed(self) -> bool {
        self.0 & Self::PEER_CLOSED.0 != 0
    }
}

impl BitOr for Signals {
    type Output = Signals;
    fn bitor(self, rhs: Signals) -> Signals {
        Signals(self.0 | rhs.0)
    }
}

/// An opaque, server-chosen token naming a registered source. The reactor
/// returns it from `wait` so the server dispatches without ever seeing a bit.
pub type Key = usize;

verus! {

/// `register`/`register_bound` failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterErr {
    /// No free bit — at most 64 sources for the MVP (bit-groups come with scale).
    Full,
    /// A `Transport::bind` returned a kernel error.
    Bind(i64),
    /// A `register_bound` requested a bit already allocated to another source.
    Taken,
}

/// Width of the notification word — the MVP per-thread source limit (rev2§3.6).
const WORD_BITS: usize = 64;

#[derive(Debug, Clone, Copy)]
struct Reg {
    key: Key,
    signals: Signals,
}

// The reactor's dispatch invariant (rev2§3.6): a slot is registered **iff** its
// `used` bit is set — the bijection between live sources and claimed bits that
// rules out double-allocation and orphaned slots. The proptest
// `register_sequence_keeps_used_coherent` is the companion oracle; this is its
// deductive, all-inputs twin (`Seq` view of the `[Option<Reg>; 64]` array).
spec fn coherent(slots: Seq<Option<Reg>>, used: u64) -> bool {
    forall|b: int|
        #![trigger slots[b]]
        0 <= b < 64 ==> (slots[b].is_some() <==> used & (1u64 << (b as u64)) != 0u64)
}

// The reactor's whole **dispatch invariant**: the 64-slot array stays full-width and
// slot/used `coherent`. Both registration paths (`register_into`, `register_bound_into`)
// take it as `requires`/`ensures`, so coherence is preserved by every source admission
// — the deductive twin of the `register_sequence_keeps_used_coherent` proptest. `pending`
// carries no clause here: it may hold signaled-but-unregistered bits (`wait` skips them),
// so its only discipline is `drain_one`'s lowest-first single-step `ensures`, not a
// coherence relation with `slots`/`used`.
spec fn wf(slots: Seq<Option<Reg>>, used: u64) -> bool {
    &&& slots.len() == 64
    &&& coherent(slots, used)
}

// The reactor's bit allocator (rev2§3.6): the lowest free bit of `used`, marked
// allocated, or `None` when the 64-bit word is full. The lowest-clear scan is the
// verified `lowest_clear_bit`; this records the allocation (`used |= 1 << bit`)
// and proves it flips *exactly* that bit — no double-allocation, nothing else
// moves. `register_into` (the verified register-path helper) delegates here.
fn alloc_lowest(used: &mut u64) -> (r: Option<usize>)
    ensures
        r is None ==> *final(used) == *old(used) && *old(used) == 0xFFFF_FFFF_FFFF_FFFFu64,
        r matches Some(bit) ==> {
            &&& bit < 64
            &&& *old(used) & (1u64 << (bit as u64)) == 0u64
            &&& *final(used) == *old(used) | (1u64 << (bit as u64))
            &&& forall|j: u64|
                #![trigger *old(used) & (1u64 << j)]
                j < bit ==> *old(used) & (1u64 << j) != 0u64
        },
{
    let bit = match lowest_clear_bit(*used) {
        Some(b) => b as usize,
        None => return None,
    };
    *used = *used | (1u64 << bit);
    Some(bit)
}

// Admit a channel source on the **lowest clear** bit (rev2§3.6) — the verified twin of
// `register_bound_into` for the auto-allocated `register` path. Pick the lowest free bit
// (the verified `alloc_lowest`), fill exactly that slot with the source's `key`/`signals`,
// and preserve the slot/used `wf` bijection: `Full` (state unchanged) iff the 64-bit word
// is exhausted, otherwise exactly that bit set in `used` and exactly that slot filled — no
// double-allocation, nothing else moves. `Reactor::register` (the plain shell) commits the
// returned `(slots, used)` only **after** its `Transport::bind`s succeed, so a bind failure
// leaves the reactor entirely untouched. The proptest `register_sequence_keeps_used_coherent`
// is the companion oracle; this is its deductive, all-inputs twin for the register path.
fn register_into(slots: [Option<Reg>; WORD_BITS], used: u64, signals: Signals, key: Key) -> (r: (
    [Option<Reg>; WORD_BITS],
    u64,
    Result<usize, RegisterErr>,
))
    requires
        wf(slots@, used),
    ensures
        wf(r.0@, r.1),
        match r.2 {
            Err(e) => {
                &&& e == RegisterErr::Full
                &&& used == 0xFFFF_FFFF_FFFF_FFFFu64
                &&& r.1 == used
                &&& r.0@ == slots@
            },
            Ok(bit) => {
                &&& bit < 64
                &&& used & (1u64 << (bit as u64)) == 0u64
                &&& r.1 == used | (1u64 << (bit as u64))
                &&& forall|j: u64|
                    #![trigger used & (1u64 << j)]
                    j < bit ==> used & (1u64 << j) != 0u64
                &&& r.0@ == slots@.update(bit as int, Some(Reg { key, signals }))
            },
        },
{
    broadcast use vstd::array::group_array_axioms;

    let mut u = used;
    match alloc_lowest(&mut u) {
        // Full word: nothing claimed, state returned unchanged (`u == used == MAX`).
        None => (slots, used, Err(RegisterErr::Full)),
        Some(bit) => {
            let mut out = slots;
            out[bit] = Some(Reg { key, signals });
            let ghost g: u64 = bit as u64;
            proof {
                // `out` is `slots` with index `bit` set to `Some(..)` (array set axiom).
                assert(out@ =~= slots@.update(bit as int, Some(Reg { key, signals })));
                // Re-establish coherence. The only changed slot is `bit`, which is exactly
                // the bit `alloc_lowest` set in `u` (`u == used | (1<<bit)`); OR-ing in
                // `1<<bit` leaves every other bit of `used` — hence every other slot's
                // membership — unchanged. Case split on `b == bit` vs `b != bit`.
                assert forall|b: int| 0 <= b < 64 implies ((#[trigger] out@[b]).is_some() <==> u & (
                1u64 << (b as u64)) != 0u64) by {
                    let bb = b as u64;
                    if b == g as int {
                        // Newly filled slot; its `used` bit is the one just set. The
                        // self identity gives `(used | (1<<g)) & (1<<g) != 0`; bridge
                        // through `u == used | (1<<g)` and `bb == g`.
                        lemma_set_bit_self(used, g);
                        assert(u & (1u64 << bb) != 0u64);
                    } else {
                        // Untouched slot: membership carries over from the entry coherence,
                        // and bit `bb` of `u` equals bit `bb` of `used` (OR didn't touch it).
                        // The other identity gives `(used | (1<<g)) & (1<<bb) == used &
                        // (1<<bb)`; bridge through `u == used | (1<<g)`.
                        assert(out@[b] == slots@[b]);
                        lemma_bit_other(used, g, bb);
                        assert((u & (1u64 << bb) != 0u64) == (used & (1u64 << bb) != 0u64));
                    }
                }
            }
            (out, u, Ok(bit))
        },
    }
}

// ORing bit `k` into `x` reads back set when masked with `1<<k` (the single-bit
// OR-set self identity behind `register`'s `used |= 1<<bit`). Pure `bit_vector`.
proof fn lemma_set_bit_self(x: u64, k: u64)
    by (bit_vector)
    requires
        k < 64,
    ensures
        (x | (1u64 << k)) & (1u64 << k) != 0u64,
{
}

// ORing bit `k` into `x` leaves every other bit `m != k` of the word unchanged, so
// an untouched slot's membership carries over the OR. The mask-equal form (the masked
// words are equal, not merely both (non)zero). Pure `bit_vector`.
proof fn lemma_bit_other(x: u64, k: u64, m: u64)
    by (bit_vector)
    requires
        k < 64,
        m < 64,
        k != m,
    ensures
        (x | (1u64 << k)) & (1u64 << m) == x & (1u64 << m),
{
}

// Popping the lowest set bit (`bits & (bits - 1)`) of a word whose lowest set bit
// is `bit` clears exactly that bit and strictly shrinks the word. The hypotheses
// pin `bit` as the lowest set bit: it is set (`bits & (1<<bit) != 0`) and the
// `bit` bits below it are clear (`bits << (64-bit) == 0`, the axiom's aggregate
// trailing-zeros form). Pure `bit_vector`; drives the `register_bound_into` drain
// loop's `decreases` and its slot-coherence step.
proof fn lemma_pop_lowest(bits: u64, bit: u64)
    by (bit_vector)
    requires
        bit < 64,
        bits & (1u64 << bit) != 0u64,
        bits << sub(64u64, bit) == 0u64,
    ensures
        bits & sub(bits, 1u64) == bits & !(1u64 << bit),
        bits & sub(bits, 1u64) < bits,
{
}

// Claim the caller-chosen `mask` bits in one externally-bound, edge-triggered
// registration (rev2§3.6): refuse (`Taken`, state unchanged) if any requested bit
// is already allocated, else set exactly `mask` in `used` and fill exactly those
// slots, preserving the slot/used `coherent` bijection for *all* inputs. The
// scan pops set bits low-to-high via `bits & (bits - 1)`; the proof tracks
// `mask & !bits` (the processed bits) and re-establishes coherence per step.
// `Reactor::register_bound` (the plain shell over the slot array) delegates here;
// the proptest `register_sequence_keeps_used_coherent` is the companion oracle.
fn register_bound_into(slots: [Option<Reg>; WORD_BITS], used: u64, mask: u64, key: Key) -> (r: (
    [Option<Reg>; WORD_BITS],
    u64,
    Result<(), RegisterErr>,
))
    requires
        wf(slots@, used),
    ensures
        wf(r.0@, r.1),
        match r.2 {
            Err(e) => {
                &&& e == RegisterErr::Taken
                &&& mask & used != 0u64
                &&& r.1 == used
                &&& r.0@ == slots@
            },
            Ok(()) => {
                &&& mask & used == 0u64
                &&& r.1 == used | mask
                &&& forall|b: int|
                    #![trigger r.0@[b]]
                    0 <= b < 64 ==> r.0@[b] == (if mask & (1u64 << (b as u64)) != 0u64 {
                        Some(Reg { key, signals: Signals(0) })
                    } else {
                        slots@[b]
                    })
            },
        },
{
    broadcast use vstd::std_specs::bits::axiom_u64_trailing_zeros;
    broadcast use vstd::array::group_array_axioms;

    if mask & used != 0 {
        return (slots, used, Err(RegisterErr::Taken));
    }
    let mut out = slots;
    let new_used = used | mask;
    let mut bits = mask;
    // Loop entry: `bits == mask`, so the processed set `mask & !bits` is empty and
    // every slot still holds its entry value (`out == slots`).
    assert(mask & !mask == 0u64) by (bit_vector);
    assert forall|b: int| 0 <= b < 64 implies #[trigger] out@[b] == (if (mask & !mask) & (1u64 << (
    b as u64)) != 0u64 {
        Some(Reg { key, signals: Signals(0) })
    } else {
        slots@[b]
    }) by {
        let bb = b as u64;
        assert((mask & !mask) & (1u64 << bb) == 0u64) by (bit_vector);
    }
    while bits != 0
        invariant
            out@.len() == 64,
            new_used == used | mask,
            bits & !mask == 0u64,
            forall|b: int|
                #![trigger out@[b]]
                0 <= b < 64 ==> out@[b] == (if (mask & !bits) & (1u64 << (b as u64)) != 0u64 {
                    Some(Reg { key, signals: Signals(0) })
                } else {
                    slots@[b]
                }),
        decreases bits,
    {
        let bit = bits.trailing_zeros();
        let ghost g = bit as u64;
        proof {
            // From the axiom (bits != 0): bit < 64, the bit is set, the lower
            // `bit` bits are clear (so `bit` is the lowest set bit).
            assert(bit < 64);
            assert((bits >> g) & 1u64 == 1u64);
            assert(bits << sub(64u64, g) == 0u64);
            assert(bits & (1u64 << g) != 0u64) by (bit_vector)
                requires
                    g < 64,
                    (bits >> g) & 1u64 == 1u64,
            ;
        }
        let newbits = bits & (bits - 1);
        proof {
            lemma_pop_lowest(bits, g);
            // `bits - 1` (exec, `bits != 0`) is `sub(bits, 1)`, so the lemma pins
            // `newbits` as `bits` with `bit` cleared, and `newbits < bits` (the
            // `decreases`).
            assert(newbits == bits & !(1u64 << g));
            // `bit` is in `mask` (the loop keeps `bits ⊆ mask`).
            assert(mask & (1u64 << g) != 0u64) by (bit_vector)
                requires
                    bits & !mask == 0u64,
                    bits & (1u64 << g) != 0u64,
            ;
            // Clearing a bit keeps `bits ⊆ mask`.
            assert(newbits & !mask == 0u64) by (bit_vector)
                requires
                    bits & !mask == 0u64,
                    newbits == bits & !(1u64 << g),
            ;
        }
        out[bit as usize] = Some(Reg { key, signals: Signals(0) });
        proof {
            // Re-establish the slot invariant for `newbits = bits & !(1<<bit)`:
            // index `bit` is now filled and moved into the processed set
            // (`mask & !newbits`); every other index is untouched, and its
            // membership in the processed set is unchanged because `newbits`
            // differs from `bits` only at `bit`.
            assert forall|b: int| 0 <= b < 64 implies #[trigger] out@[b] == (if (mask & !newbits)
                & (1u64 << (b as u64)) != 0u64 {
                Some(Reg { key, signals: Signals(0) })
            } else {
                slots@[b]
            }) by {
                let bb = b as u64;
                if b == g as int {
                    assert((mask & !newbits) & (1u64 << bb) != 0u64) by (bit_vector)
                        requires
                            newbits == bits & !(1u64 << g),
                            mask & (1u64 << g) != 0u64,
                            bb == g,
                    ;
                } else {
                    assert(bb != g);
                    assert(((mask & !newbits) & (1u64 << bb) != 0u64) == ((mask & !bits) & (1u64
                        << bb) != 0u64)) by (bit_vector)
                        requires
                            newbits == bits & !(1u64 << g),
                            bb != g,
                            bb < 64,
                            g < 64,
                    ;
                }
            }
        }
        bits = newbits;
    }
    proof {
        // Loop exit: `bits == 0`, so `mask & !bits == mask` and `out` is filled at
        // exactly `mask`'s bits. Coherence of `(out, new_used)` follows from the
        // entry coherence of `(slots, used)` and `new_used == used | mask`.
        assert(mask & !0u64 == mask) by (bit_vector);
        assert forall|b: int| 0 <= b < 64 implies ((#[trigger] out@[b]).is_some() <==> new_used & (
        1u64 << (b as u64)) != 0u64) by {
            let bb = b as u64;
            if mask & (1u64 << bb) != 0u64 {
                assert(new_used & (1u64 << bb) != 0u64) by (bit_vector)
                    requires
                        new_used == used | mask,
                        mask & (1u64 << bb) != 0u64,
                ;
            } else {
                assert((new_used & (1u64 << bb) != 0u64) == (used & (1u64 << bb) != 0u64))
                    by (bit_vector)
                    requires
                        new_used == used | mask,
                        mask & (1u64 << bb) == 0u64,
                ;
            }
        }
    }
    (out, new_used, Ok(()))
}

} // verus!
/// The reactor: waits on one notification multiplexing many sources.
pub struct Reactor<'t, T: Transport> {
    transport: &'t T,
    notif: Notif,
    slots: [Option<Reg>; WORD_BITS],
    /// Allocated bits. A source owns a bit for life (there is no deregister), so
    /// the lowest clear bit is the next free one — `register` allocates that way,
    /// and `register_bound` claims caller-chosen bits, both recorded here.
    used: u64,
    /// Set bits observed by the last `notif_wait` but not yet returned — drained
    /// across `wait` calls so several ready sources surface without re-signaling.
    pending: u64,
}

impl<'t, T: Transport> Reactor<'t, T> {
    /// A reactor over `notif`, the notification all its sources bind into.
    pub fn new(transport: &'t T, notif: Notif) -> Reactor<'t, T> {
        Reactor {
            transport,
            notif,
            slots: [None; WORD_BITS],
            used: 0,
            pending: 0,
        }
    }

    /// Register `source` for `signals`, dispatched as `key`. Picks the lowest free
    /// bit and fills its slot via the Verus-verified [`register_into`] (which proves
    /// the slot/used coherence bijection — no double-allocation, `Full` at the 64-bit
    /// ceiling), binds each requested event to that bit, then **self-signals** it so
    /// the first `wait` polls the source (the "poll once", catching a pre-bind
    /// message). Idempotent re-registration is not supported — each call consumes a
    /// bit.
    ///
    /// The verified `(slots, used)` is committed only **after** every `bind` succeeds,
    /// so a `bind` failure leaves the reactor entirely unchanged (the bit is not even
    /// consumed) — the slot/used coherence holds on every path.
    pub fn register(
        &mut self,
        source: Chan,
        signals: Signals,
        key: Key,
    ) -> Result<(), RegisterErr> {
        // Verified, pure: pick the lowest clear bit and the would-be new state, without
        // touching `self` yet. `Full` (word exhausted) returns here, self untouched.
        let (slots, used, r) = register_into(self.slots, self.used, signals, key);
        let bit = r?;
        let mask = 1u64 << bit;

        // Bind before committing: a bind failure returns with `self` unchanged.
        if signals.readable() {
            self.bind(source, Event::Readable, mask)?;
        }
        if signals.writable() {
            self.bind(source, Event::Writable, mask)?;
        }
        if signals.peer_closed() {
            self.bind(source, Event::PeerClosed, mask)?;
        }

        // All binds succeeded: commit the verified-coherent allocation.
        self.slots = slots;
        self.used = used;
        // Poll once: surface this source on the first wait, so a message already
        // queued before the bind is not slept through.
        self.transport.notif_signal(self.notif, mask);
        Ok(())
    }

    /// Register a source whose events are bound to `mask` **outside** the reactor
    /// and are **edge-triggered**: a thread on-exit/on-fault binding (a
    /// `thread_bind` into the TCB, rev2§5.1), a timer (armed via `sys::timer_arm`),
    /// or an IRQ — anything the kernel signals into this notification at a bit
    /// the caller controls. Each set bit in `mask` dispatches to `key`.
    ///
    /// Unlike [`Self::register`], this does **no** `bind` (it is not a channel event)
    /// and does **no** poll-once self-signal: an edge-triggered source fires
    /// exactly once when the event actually happens, so a fabricated poll-once
    /// would deliver a spurious wakeup (e.g. report a thread dead before it is).
    /// The reactor therefore owns only the bit→key dispatch and the
    /// word-check-before-block half of the lost-wakeup discipline; **the caller
    /// must bind the source before it can fire** (e.g. `SpawnRec::arm` before
    /// `start`), so a `wait` cannot sleep through a signal that already arrived.
    ///
    /// `Err(Taken)` if any requested bit is already allocated.
    pub fn register_bound(&mut self, mask: u64, key: Key) -> Result<(), RegisterErr> {
        // The bit-scan + slot/used coherence guarantees are the Verus-verified
        // [`register_bound_into`]; this shell threads the slot array and `used`
        // mask through it. Bound sources carry no channel signals — the key alone
        // names them (`register_bound_into` fills `Signals(0)`).
        let (slots, used, r) = register_bound_into(self.slots, self.used, mask, key);
        self.slots = slots;
        self.used = used;
        r
    }

    fn bind(&self, source: Chan, ev: Event, mask: u64) -> Result<(), RegisterErr> {
        self.transport
            .bind(source, ev, self.notif, mask)
            .map_err(RegisterErr::Bind)
    }

    /// Block until a registered source is ready, returning its `(key, signals)`.
    /// The returned `signals` are the source's *registered* set (a level-drain
    /// hint, not a precise per-event readiness); the caller polls (`recv_nb` for
    /// readable, `send_nb` for writable) and re-`wait`s on a spurious wakeup.
    pub fn wait(&mut self) -> (Key, Signals) {
        loop {
            if self.pending == 0 {
                // notif_wait returns the accumulated word (non-zero) and clears
                // it, blocking only while it is zero — the lost-wakeup guard.
                self.pending = self.transport.notif_wait(self.notif);
            }
            let (bit, rest) = drain_one(self.pending);
            self.pending = rest;
            let bit = bit as usize;
            if let Some(reg) = self.slots[bit] {
                return (reg.key, reg.signals);
            }
            // A set bit with no registration: ignore and keep draining/waiting.
        }
    }
}

// Sequential-dispatch property tests (rev2§6 baseline tier): the
// `used`-mask bit allocation, the `pending` drain, and the lowest-bit
// `trailing_zeros` scan are single-threaded state-machine logic — the reactor
// mutates them under the holder's own thread, so their tier is proptest + Miri,
// not a concurrency harness (the *concurrent* protocol is the Loom/Shuttle
// harnesses' in `model.rs` and the `tla/ipc_reactor` model's). `ipc/` is
// atomics-free, so Loom adds nothing here. Std-only: gated off the loom/shuttle
// model builds (the workspace idiom, `urt/src/time.rs`).
#[cfg(all(test, not(loom), not(shuttle)))]
mod proptests {
    use super::*;
    use crate::model::ModelTransport;
    use proptest::prelude::*;

    const NOTIF: crate::transport::Notif = 0;
    const CHAN: Chan = 0;

    // A registration step driving the `used`-mask allocator.
    #[derive(Debug, Clone)]
    enum Op {
        /// `register` a channel for READABLE — allocates the *lowest clear* bit.
        Register,
        /// `register_bound` with a caller-chosen mask — claims exactly those bits.
        Bound(u64),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            // Bias toward `Register` so sequences reach the 64-bit ceiling.
            3 => Just(Op::Register),
            1 => any::<u64>().prop_map(Op::Bound),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            failure_persistence: if cfg!(miri) { None } else { ProptestConfig::default().failure_persistence },
            .. ProptestConfig::default()
        })]

        /// `alloc_lowest` (the verified lowest-clear allocator core `register`/
        /// `register_into` delegate to) returns the **lowest clear** bit
        /// (characterized, not compared to its own impl): the returned bit was
        /// clear, every lower bit was already set, and exactly that bit flips. It
        /// refuses (`None`) only when the word is full — never aliases, never panics.
        #[test]
        fn alloc_bit_is_lowest_clear(used in any::<u64>()) {
            let mut u = used;
            let before = u;
            match alloc_lowest(&mut u) {
                Some(bit) => {
                    prop_assert!(bit < WORD_BITS);
                    prop_assert_eq!(before & (1u64 << bit), 0, "allocated an already-set bit");
                    // Lowest clear: all bits below `bit` were set.
                    let below = (1u64 << bit) - 1;
                    prop_assert_eq!(before & below, below, "a lower bit was clear — not lowest-first");
                    // Exactly that bit is added; nothing else moves.
                    prop_assert_eq!(u, before | (1u64 << bit));
                }
                None => prop_assert_eq!(before, u64::MAX, "refused while a bit was still free"),
            }
        }

        /// Over an arbitrary `register`/`register_bound` sequence: `used` stays
        /// equal to the union of all claimed bits (the bitmap-coherence
        /// invariant — a bijection between live sources and set bits, no
        /// double-allocation), `slots[bit].is_some()` iff the bit is used, every
        /// `register` takes the lowest clear bit, a `register_bound` of an
        /// already-used mask is `Taken` (leaving `used` untouched), and a
        /// `register` past the 64-bit ceiling is `Full` (no alias, no panic).
        #[test]
        fn register_sequence_keeps_used_coherent(ops in prop::collection::vec(op_strategy(), 0..80)) {
            let t = ModelTransport::shared(1, 1);
            let mut reactor = Reactor::new(&*t, NOTIF);
            let mut live: u64 = 0; // the oracle mask of claimed bits
            let mut next_key: Key = 0;

            for op in ops {
                match op {
                    Op::Register => {
                        let before = reactor.used;
                        prop_assert_eq!(before, live, "used drifted from the oracle");
                        let r = reactor.register(CHAN, Signals::READABLE, next_key);
                        next_key += 1;
                        if live == u64::MAX {
                            // Full word: refuse cleanly, nothing claimed.
                            prop_assert_eq!(r, Err(RegisterErr::Full));
                            prop_assert_eq!(reactor.used, before);
                        } else {
                            prop_assert!(r.is_ok());
                            // Exactly one new bit, and it is the lowest clear one.
                            let added = reactor.used ^ before;
                            prop_assert_eq!(added.count_ones(), 1u32);
                            let bit = added.trailing_zeros();
                            prop_assert_eq!(bit, (!before).trailing_zeros(), "register skipped a lower free bit");
                            live |= added;
                        }
                    }
                    Op::Bound(mask) => {
                        let before = reactor.used;
                        let r = reactor.register_bound(mask, next_key);
                        next_key += 1;
                        if mask & live != 0 {
                            prop_assert_eq!(r, Err(RegisterErr::Taken));
                            prop_assert_eq!(reactor.used, before, "Taken must leave used unchanged");
                        } else {
                            prop_assert!(r.is_ok());
                            live |= mask;
                            prop_assert_eq!(reactor.used, live);
                        }
                    }
                }
                // Coherence after every step: a slot is registered iff its bit is used.
                for bit in 0..WORD_BITS {
                    let set = live & (1u64 << bit) != 0;
                    prop_assert_eq!(reactor.slots[bit].is_some(), set, "slot/used incoherent at bit {}", bit);
                }
            }
        }

        /// The `pending` drain over an arbitrary signaled mask: `wait` yields
        /// exactly the *registered* set bits, each once, in lowest-first
        /// (`trailing_zeros`) order, mapping each to its `(key, signals)` — the
        /// epoll-shaped O(1) dispatch (rev2§3.6). Signaled-but-unregistered bits
        /// (`u`) are silently skipped, never returned and never blocking. `m`
        /// and `u` are disjoint subsets so `wait` is called exactly `|m|` times
        /// and never sleeps.
        #[test]
        fn pending_drain_is_lowest_first(seed_s in any::<u64>(), seed_m in any::<u64>(), seed_u in any::<u64>()) {
            let s = seed_s; // registered bits
            let m = seed_m & s; // signaled *and* registered
            let u = seed_u & !s; // signaled *but not* registered
            let t = ModelTransport::shared(1, 1);
            let mut reactor = Reactor::new(&*t, NOTIF);
            // Register each bit of S to a distinct key (key == bit) so the
            // returned key order reveals the drain order.
            for bit in 0..WORD_BITS {
                if s & (1u64 << bit) != 0 {
                    reactor.register_bound(1u64 << bit, bit as Key).unwrap();
                }
            }
            // Signal the registered subset plus the unregistered noise.
            t.notif_signal(NOTIF, m | u);

            let mut got: std::vec::Vec<Key> = std::vec::Vec::new();
            for _ in 0..m.count_ones() {
                let (key, signals) = reactor.wait();
                prop_assert_eq!(signals, Signals(0), "register_bound sources carry no signals");
                got.push(key);
            }
            let expected: std::vec::Vec<Key> =
                (0..WORD_BITS).filter(|&b| m & (1u64 << b) != 0).collect();
            prop_assert_eq!(got, expected, "drain not lowest-first / not exactly the signaled-registered set");
        }
    }

    /// The 64-bit structural ceiling (rev2§3.6): exactly `WORD_BITS` `register`s
    /// succeed on distinct bits 0..64, and the 65th refuses with `Full` — no
    /// alias, no panic. (The proptest reaches this state probabilistically; this
    /// pins the boundary deterministically.)
    #[cfg(not(miri))]
    #[test]
    fn alloc_exhausts_at_word_bits() {
        let t = ModelTransport::shared(1, 1);
        let mut reactor = Reactor::new(&*t, NOTIF);
        for k in 0..WORD_BITS {
            assert!(reactor.register(CHAN, Signals::READABLE, k).is_ok());
        }
        assert_eq!(reactor.used, u64::MAX, "all 64 bits claimed");
        assert_eq!(
            reactor.register(CHAN, Signals::READABLE, 999),
            Err(RegisterErr::Full),
            "the 65th register must refuse"
        );
        assert_eq!(reactor.used, u64::MAX, "a refused register claims nothing");
    }
}
