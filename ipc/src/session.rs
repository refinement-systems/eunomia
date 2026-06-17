//! Sessions and connect (plan `doc/plans/2_ipc.md` §4.6; spec §3.5) — the
//! **admission** layer. A client funds the channel (retypes it from its own
//! untyped, §3.2) and sends a `ConnectReq` naming a requested **bulk-window
//! size**; the server grants or refuses at a **single admission point**, bounded
//! by its per-server window quota. Queue memory is the connector's; window
//! memory is the server's, capped by the quota it enforces here — so an
//! anonymous connect cannot drain the server (§3.5, "fund by failure mode").
//!
//! The genuinely-new, safety-bearing logic is [`Admission`]: it never grants
//! past its budget (the quota invariant) and returns the quota on session
//! close. The wire forms ([`ConnectReq`], [`GrantReply`]) are fixed,
//! hand-written little-endian codecs in the spirit of [`crate::header`] — boring
//! and byte-stable, so this layer stays in the default `no_std` build (no
//! postcard, no `alloc`). The transport round-trip itself is the already-proven
//! `Endpoint::{send_nb, recv_nb}`; the server runs the decode→admit→reply step
//! ([`admit_connect`]) inside its reactor loop exactly as it runs any request.
//!
//! **Verified by Verus** (plan `doc/plans/3_verus-rewrite_phase7-detail.md` §7b —
//! the §4.6 session layer of the §4.7 host chokepoints, after the 7a header
//! pilot). The codecs are total bijections (`[`req_encode`]`/`[`req_decode`]`,
//! `[`grant_encode`]`/`[`grant_decode`]` as ghost models, the round-trip lemmas
//! ∀); and [`Admission`] **never over-grants for all admit/release sequences** —
//! `granted <= budget` ([`Admission::well_formed`]) is a pre/post-condition of
//! every op, proven once and composed over *any* sequence by Verus's modular
//! reasoning, so `remaining()`'s `budget - granted` is non-underflowing (vs the
//! bounded 3-step Kani harness this supersedes). As in `header.rs` the exec
//! codecs use explicit mask/shift arithmetic (not `to_le_bytes`/`copy_from_slice`,
//! which Verus does not spec), so `vstd` stays ghost-only and erases into the
//! alloc-free user binaries; the bytes produced are unchanged.

use vstd::prelude::*;

verus! {

// Tag bytes for the fixed wire forms (first payload byte). Distinct so a
// decoder rejects a message of the wrong kind rather than misreading it. `pub`
// so the `pub open spec` codec models below can name them (the `verus!{}`
// rule: an open spec body cannot reference a private item; cf. `header::HEADER_SIZE`).
pub const TAG_REQ: u8 = 0xC0;
pub const TAG_GRANT: u8 = 0x01;
pub const TAG_REFUSED: u8 = 0x00;

pub const REQ_LEN: usize = 5; // tag + u32
pub const GRANT_LEN: usize = 9; // tag + u32 window + u32 size
pub const REFUSED_LEN: usize = 1; // tag

/// A granted bulk window (§3.1, §4.6): which window and how many bytes. The MVP
/// grants a single window, so `window` is always 0; it exists so the descriptor
/// ABI is grow-only when multi-window lands (§9, out of scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGrant {
    pub window: u32,
    pub size: u32,
}

/// A connect request (§4.6): the client's requested bulk-window size in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectReq {
    pub requested_window: u32,
}

/// The server's reply to a connect (§4.6): a granted window, or a refusal when
/// the request does not fit the remaining quota (the single admission point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantReply {
    Grant(WindowGrant),
    Refused,
}

/// Why a connect failed. Today the only failure is the server refusing under its
/// window quota (`admit`'s sole error); the client-side connect *mechanism* (the
/// endpoint-cap handshake, §3.5) is deferred, so its richer errors — a
/// peer-closed session channel, a reply that does not decode, a transport error
/// — are not yet constructed. They return when that mechanism lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectErr {
    /// The server refused under its quota (§3.5).
    Refused,
}

// ── Ghost models of the codecs (the little-endian byte layout as a `Seq`) ──

/// Ghost model of [`ConnectReq::encode`]: tag byte then the requested-window
/// `u32` split low-to-high (matching `to_le_bytes`).
pub open spec fn req_encode(r: ConnectReq) -> Seq<u8> {
    seq![
        TAG_REQ,
        (r.requested_window & 0xff) as u8,
        ((r.requested_window >> 8) & 0xff) as u8,
        ((r.requested_window >> 16) & 0xff) as u8,
        ((r.requested_window >> 24) & 0xff) as u8,
    ]
}

/// Ghost model of [`ConnectReq::decode`]: `Some` iff exactly `REQ_LEN` bytes
/// tagged `TAG_REQ`, reassembling the little-endian `u32`; `None` otherwise.
/// Total over every byte string.
pub open spec fn req_decode(s: Seq<u8>) -> Option<ConnectReq> {
    if s.len() == REQ_LEN && s[0] == TAG_REQ {
        Some(ConnectReq {
            requested_window: (s[1] as u32) | ((s[2] as u32) << 8) | ((s[3] as u32) << 16)
                | ((s[4] as u32) << 24),
        })
    } else {
        None
    }
}

/// Ghost model of [`GrantReply::encode`]'s *used prefix*: a `GRANT_LEN` grant
/// (tag + window + size, each little-endian) or a `REFUSED_LEN` refusal (tag).
pub open spec fn grant_encode(g: GrantReply) -> Seq<u8> {
    match g {
        GrantReply::Grant(w) => seq![
            TAG_GRANT,
            (w.window & 0xff) as u8,
            ((w.window >> 8) & 0xff) as u8,
            ((w.window >> 16) & 0xff) as u8,
            ((w.window >> 24) & 0xff) as u8,
            (w.size & 0xff) as u8,
            ((w.size >> 8) & 0xff) as u8,
            ((w.size >> 16) & 0xff) as u8,
            ((w.size >> 24) & 0xff) as u8,
        ],
        GrantReply::Refused => seq![TAG_REFUSED],
    }
}

/// Ghost model of [`GrantReply::decode`]: a `GRANT_LEN` grant (tag `TAG_GRANT`)
/// or a `REFUSED_LEN` refusal (tag `TAG_REFUSED`); `None` otherwise. Total.
pub open spec fn grant_decode(s: Seq<u8>) -> Option<GrantReply> {
    if s.len() == GRANT_LEN && s[0] == TAG_GRANT {
        Some(GrantReply::Grant(WindowGrant {
            window: (s[1] as u32) | ((s[2] as u32) << 8) | ((s[3] as u32) << 16)
                | ((s[4] as u32) << 24),
            size: (s[5] as u32) | ((s[6] as u32) << 8) | ((s[7] as u32) << 16)
                | ((s[8] as u32) << 24),
        }))
    } else if s.len() == REFUSED_LEN && s[0] == TAG_REFUSED {
        Some(GrantReply::Refused)
    } else {
        None
    }
}

impl ConnectReq {
    /// A request for a `requested` byte bulk window.
    pub fn for_window(requested: u32) -> (r: ConnectReq)
        ensures r.requested_window == requested,
    {
        ConnectReq { requested_window: requested }
    }

    pub fn encode(&self) -> (b: [u8; REQ_LEN])
        ensures b@ == req_encode(*self),
    {
        broadcast use vstd::array::group_array_axioms;
        let b: [u8; REQ_LEN] = [
            TAG_REQ,
            (self.requested_window & 0xff) as u8,
            ((self.requested_window >> 8) & 0xff) as u8,
            ((self.requested_window >> 16) & 0xff) as u8,
            ((self.requested_window >> 24) & 0xff) as u8,
        ];
        assert(b@ =~= req_encode(*self));
        b
    }

    /// Decode exactly `REQ_LEN` bytes tagged `TAG_REQ`; reject any other length
    /// or a bad tag (total over byte values, like [`crate::header::Header::decode`]).
    pub fn decode(buf: &[u8]) -> (r: Option<ConnectReq>)
        ensures
            r == req_decode(buf@),
            r is Some <==> (buf@.len() == REQ_LEN && buf@[0] == TAG_REQ),
    {
        broadcast use vstd::slice::group_slice_axioms;
        if buf.len() != REQ_LEN || buf[0] != TAG_REQ {
            return None;
        }
        Some(ConnectReq {
            requested_window: (buf[1] as u32) | ((buf[2] as u32) << 8) | ((buf[3] as u32) << 16)
                | ((buf[4] as u32) << 24),
        })
    }
}

impl GrantReply {
    pub fn encode(&self) -> (res: ([u8; GRANT_LEN], usize))
        ensures
            res.1 == grant_encode(*self).len(),
            res.0@.subrange(0, res.1 as int) == grant_encode(*self),
    {
        broadcast use vstd::array::group_array_axioms;
        match *self {
            GrantReply::Grant(g) => {
                let b: [u8; GRANT_LEN] = [
                    TAG_GRANT,
                    (g.window & 0xff) as u8,
                    ((g.window >> 8) & 0xff) as u8,
                    ((g.window >> 16) & 0xff) as u8,
                    ((g.window >> 24) & 0xff) as u8,
                    (g.size & 0xff) as u8,
                    ((g.size >> 8) & 0xff) as u8,
                    ((g.size >> 16) & 0xff) as u8,
                    ((g.size >> 24) & 0xff) as u8,
                ];
                assert(b@.subrange(0, GRANT_LEN as int) =~= grant_encode(*self));
                (b, GRANT_LEN)
            }
            GrantReply::Refused => {
                let b: [u8; GRANT_LEN] = [TAG_REFUSED, 0, 0, 0, 0, 0, 0, 0, 0];
                assert(b@.subrange(0, REFUSED_LEN as int) =~= grant_encode(*self));
                (b, REFUSED_LEN)
            }
        }
    }

    /// Decode a reply: a `GRANT_LEN` grant or a `REFUSED_LEN` refusal; reject
    /// anything else (total over byte values).
    pub fn decode(buf: &[u8]) -> (r: Option<GrantReply>)
        ensures
            r == grant_decode(buf@),
            r is Some <==> ((buf@.len() == GRANT_LEN && buf@[0] == TAG_GRANT)
                || (buf@.len() == REFUSED_LEN && buf@[0] == TAG_REFUSED)),
    {
        broadcast use vstd::slice::group_slice_axioms;
        if buf.len() == GRANT_LEN && buf[0] == TAG_GRANT {
            Some(GrantReply::Grant(WindowGrant {
                window: (buf[1] as u32) | ((buf[2] as u32) << 8) | ((buf[3] as u32) << 16)
                    | ((buf[4] as u32) << 24),
                size: (buf[5] as u32) | ((buf[6] as u32) << 8) | ((buf[7] as u32) << 16)
                    | ((buf[8] as u32) << 24),
            }))
        } else if buf.len() == REFUSED_LEN && buf[0] == TAG_REFUSED {
            Some(GrantReply::Refused)
        } else {
            None
        }
    }
}

/// The per-server bulk-window quota (§3.5, §4.6): the **single admission point**.
/// Tracks a fixed `budget` of window bytes and how much is currently `granted`;
/// `admit` hands out a window iff it fits the remainder (it **never** over-grants
/// — the quota invariant a malicious flood of connects cannot break), and
/// `release` returns the bytes when a session closes. The MVP grants one window
/// per session, all into window 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    budget: u32,
    granted: u32,
}

impl Admission {
    /// The quota invariant: never more granted than the budget. Established by
    /// [`Admission::new`] and preserved by every `admit`/`release`, so it holds
    /// for *all* sequences — which is exactly why `remaining()` never underflows.
    /// `closed` keeps the private-field body out of the public contract.
    pub closed spec fn well_formed(self) -> bool {
        self.granted <= self.budget
    }

    /// The observable quota — window bytes still grantable — as a ghost value
    /// (non-negative under [`Admission::well_formed`]). A `closed` accessor so the
    /// public contracts can speak of the remaining budget without exposing the
    /// private `budget`/`granted` split: it is what `remaining()` returns and what
    /// `admit`/`release` move.
    pub closed spec fn spec_remaining(self) -> int {
        self.budget as int - self.granted as int
    }

    /// A quota that will grant at most `budget` window bytes in total.
    pub fn new(budget: u32) -> (a: Admission)
        ensures
            a.well_formed(),
            a.spec_remaining() == budget,
    {
        Admission { budget, granted: 0 }
    }

    /// Window bytes still available to grant. Non-underflowing under the quota
    /// invariant (`granted <= budget`).
    pub fn remaining(&self) -> (r: u32)
        requires self.well_formed(),
        ensures r == self.spec_remaining(),
    {
        self.budget - self.granted
    }

    /// The single admission decision (§3.5): grant `requested` bytes iff they fit
    /// the remaining quota, accounting for them; otherwise refuse and leave the
    /// quota untouched. **Never grants past budget** — [`Admission::well_formed`]
    /// holds after every call, for *any* `requested`, so a flood of connects can
    /// never push `granted` past `budget` (the unbounded never-over-grant theorem).
    pub fn admit(&mut self, requested: u32) -> (res: Result<WindowGrant, ConnectErr>)
        requires self.well_formed(),
        ensures
            final(self).well_formed(),
            res is Ok ==> {
                &&& res->Ok_0.window == 0
                &&& res->Ok_0.size == requested
                &&& requested <= old(self).spec_remaining()
                &&& final(self).spec_remaining() == old(self).spec_remaining() - requested
            },
            res is Err ==> {
                &&& res->Err_0 == ConnectErr::Refused
                &&& requested > old(self).spec_remaining()
                &&& final(self).spec_remaining() == old(self).spec_remaining()
            },
    {
        if requested <= self.remaining() {
            self.granted = self.granted + requested;
            Ok(WindowGrant { window: 0, size: requested })
        } else {
            Err(ConnectErr::Refused)
        }
    }

    /// Return a granted window's bytes to the quota on session close. A grant is
    /// released exactly once; releasing more than was granted is clamped to zero
    /// (defensive — a double release must not underflow the accounting), and the
    /// quota invariant is preserved either way (the returned bytes only ever
    /// raise the remaining quota).
    pub fn release(&mut self, grant: WindowGrant)
        requires self.well_formed(),
        ensures
            final(self).well_formed(),
            final(self).spec_remaining() >= old(self).spec_remaining(),
    {
        self.granted = self.granted.saturating_sub(grant.size);
    }
}

/// The server's connect step (§4.6), the admission point as a pure function:
/// decode the request bytes, decide under `adm`, and return the reply to send
/// back. A request that does not decode is refused (a server cannot grant a
/// window it cannot size). The caller does the transport round-trip — `recv_nb`
/// the request, `admit_connect`, `send_nb` the `encode`d reply — inside its
/// reactor loop, the same shape as serving any other request. Preserves the
/// quota invariant.
pub fn admit_connect(adm: &mut Admission, req_bytes: &[u8]) -> (r: GrantReply)
    requires adm.well_formed(),
    ensures final(adm).well_formed(),
{
    match ConnectReq::decode(req_bytes) {
        Some(req) => match adm.admit(req.requested_window) {
            Ok(g) => GrantReply::Grant(g),
            Err(_) => GrantReply::Refused,
        },
        None => GrantReply::Refused,
    }
}

// ── Codec bijection lemmas (∀; the bit_vector split/reassemble identities) ──

/// `decode`∘`encode` is the identity on `ConnectReq`: every request round-trips.
pub proof fn lemma_req_decode_encode(r: ConnectReq)
    ensures
        req_decode(req_encode(r)) == Some(r),
{
    let s = req_encode(r);
    assert(s.len() == REQ_LEN);
    let rw = r.requested_window;
    assert(((rw & 0xff) as u8 as u32) | (((rw >> 8) & 0xff) as u8 as u32) << 8
        | (((rw >> 16) & 0xff) as u8 as u32) << 16 | (((rw >> 24) & 0xff) as u8 as u32) << 24
        == rw) by (bit_vector);
}

/// `encode`∘`decode` is the identity on accepted request bytes. With
/// [`lemma_req_decode_encode`] this makes the request codec a total bijection
/// between `ConnectReq` values and `REQ_LEN`-byte `TAG_REQ` strings.
pub proof fn lemma_req_encode_decode(s: Seq<u8>)
    requires
        s.len() == REQ_LEN,
        s[0] == TAG_REQ,
    ensures
        req_encode(req_decode(s)->Some_0) == s,
{
    let s1 = s[1]; let s2 = s[2]; let s3 = s[3]; let s4 = s[4];
    assert((((s1 as u32) | ((s2 as u32) << 8) | ((s3 as u32) << 16) | ((s4 as u32) << 24))
        & 0xff) as u8 == s1) by (bit_vector);
    assert(((((s1 as u32) | ((s2 as u32) << 8) | ((s3 as u32) << 16) | ((s4 as u32) << 24))
        >> 8) & 0xff) as u8 == s2) by (bit_vector);
    assert(((((s1 as u32) | ((s2 as u32) << 8) | ((s3 as u32) << 16) | ((s4 as u32) << 24))
        >> 16) & 0xff) as u8 == s3) by (bit_vector);
    assert(((((s1 as u32) | ((s2 as u32) << 8) | ((s3 as u32) << 16) | ((s4 as u32) << 24))
        >> 24) & 0xff) as u8 == s4) by (bit_vector);
    assert(req_encode(req_decode(s)->Some_0) =~= s);
}

/// `decode`∘`encode` is the identity on `GrantReply` (both arms).
pub proof fn lemma_grant_decode_encode(g: GrantReply)
    ensures
        grant_decode(grant_encode(g)) == Some(g),
{
    match g {
        GrantReply::Grant(w) => {
            let win = w.window; let sz = w.size;
            assert(((win & 0xff) as u8 as u32) | (((win >> 8) & 0xff) as u8 as u32) << 8
                | (((win >> 16) & 0xff) as u8 as u32) << 16
                | (((win >> 24) & 0xff) as u8 as u32) << 24 == win) by (bit_vector);
            assert(((sz & 0xff) as u8 as u32) | (((sz >> 8) & 0xff) as u8 as u32) << 8
                | (((sz >> 16) & 0xff) as u8 as u32) << 16
                | (((sz >> 24) & 0xff) as u8 as u32) << 24 == sz) by (bit_vector);
        }
        GrantReply::Refused => {}
    }
}

/// `encode`∘`decode` is the identity on accepted reply bytes (grant + refusal).
/// With [`lemma_grant_decode_encode`] the reply codec is a total bijection
/// between `GrantReply` values and their accepted byte strings.
pub proof fn lemma_grant_encode_decode(s: Seq<u8>)
    requires
        (s.len() == GRANT_LEN && s[0] == TAG_GRANT) || (s.len() == REFUSED_LEN && s[0] == TAG_REFUSED),
    ensures
        grant_encode(grant_decode(s)->Some_0) == s,
{
    if s.len() == GRANT_LEN && s[0] == TAG_GRANT {
        let s1 = s[1]; let s2 = s[2]; let s3 = s[3]; let s4 = s[4];
        let s5 = s[5]; let s6 = s[6]; let s7 = s[7]; let s8 = s[8];
        assert((((s1 as u32) | ((s2 as u32) << 8) | ((s3 as u32) << 16) | ((s4 as u32) << 24))
            & 0xff) as u8 == s1) by (bit_vector);
        assert(((((s1 as u32) | ((s2 as u32) << 8) | ((s3 as u32) << 16) | ((s4 as u32) << 24))
            >> 8) & 0xff) as u8 == s2) by (bit_vector);
        assert(((((s1 as u32) | ((s2 as u32) << 8) | ((s3 as u32) << 16) | ((s4 as u32) << 24))
            >> 16) & 0xff) as u8 == s3) by (bit_vector);
        assert(((((s1 as u32) | ((s2 as u32) << 8) | ((s3 as u32) << 16) | ((s4 as u32) << 24))
            >> 24) & 0xff) as u8 == s4) by (bit_vector);
        assert((((s5 as u32) | ((s6 as u32) << 8) | ((s7 as u32) << 16) | ((s8 as u32) << 24))
            & 0xff) as u8 == s5) by (bit_vector);
        assert(((((s5 as u32) | ((s6 as u32) << 8) | ((s7 as u32) << 16) | ((s8 as u32) << 24))
            >> 8) & 0xff) as u8 == s6) by (bit_vector);
        assert(((((s5 as u32) | ((s6 as u32) << 8) | ((s7 as u32) << 16) | ((s8 as u32) << 24))
            >> 16) & 0xff) as u8 == s7) by (bit_vector);
        assert(((((s5 as u32) | ((s6 as u32) << 8) | ((s7 as u32) << 16) | ((s8 as u32) << 24))
            >> 24) & 0xff) as u8 == s8) by (bit_vector);
        assert(grant_encode(grant_decode(s)->Some_0) =~= s);
    } else {
        assert(s.len() == REFUSED_LEN && s[0] == TAG_REFUSED);
        assert(grant_encode(grant_decode(s)->Some_0) =~= s);
    }
}

} // verus!

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_req_roundtrip() {
        let r = ConnectReq::for_window(4096);
        assert_eq!(ConnectReq::decode(&r.encode()), Some(r));
    }

    #[test]
    fn connect_req_rejects_bad_len_and_tag() {
        assert_eq!(ConnectReq::decode(&[]), None);
        assert_eq!(ConnectReq::decode(&[TAG_REQ, 0, 0, 0]), None); // short
        assert_eq!(ConnectReq::decode(&[TAG_REQ, 0, 0, 0, 0, 0]), None); // trailing
        assert_eq!(ConnectReq::decode(&[0xFF, 0, 0, 0, 0]), None); // wrong tag
    }

    #[test]
    fn grant_reply_roundtrip() {
        let g = GrantReply::Grant(WindowGrant { window: 0, size: 8192 });
        let (b, n) = g.encode();
        assert_eq!(GrantReply::decode(&b[..n]), Some(g));

        let r = GrantReply::Refused;
        let (b, n) = r.encode();
        assert_eq!(GrantReply::decode(&b[..n]), Some(r));
    }

    #[test]
    fn grant_reply_rejects_malformed() {
        assert_eq!(GrantReply::decode(&[]), None);
        assert_eq!(GrantReply::decode(&[TAG_GRANT, 0, 0]), None); // short grant
        assert_eq!(GrantReply::decode(&[TAG_REFUSED, 0]), None); // refusal w/ trailing
        assert_eq!(GrantReply::decode(&[0x55]), None); // unknown tag
    }

    #[test]
    fn admission_grants_within_budget_and_accounts() {
        let mut adm = Admission::new(10);
        assert_eq!(adm.admit(4), Ok(WindowGrant { window: 0, size: 4 }));
        assert_eq!(adm.remaining(), 6);
        assert_eq!(adm.admit(6), Ok(WindowGrant { window: 0, size: 6 }));
        assert_eq!(adm.remaining(), 0);
    }

    #[test]
    fn admission_never_over_grants() {
        let mut adm = Admission::new(5);
        assert_eq!(adm.admit(6), Err(ConnectErr::Refused)); // does not fit
        assert_eq!(adm.remaining(), 5); // refusal leaves the quota untouched
        assert_eq!(adm.admit(5), Ok(WindowGrant { window: 0, size: 5 }));
        assert_eq!(adm.admit(1), Err(ConnectErr::Refused)); // exhausted
        // The invariant: a flood of requests never pushes granted past budget.
        let mut adm = Admission::new(3);
        for _ in 0..100 {
            let _ = adm.admit(1);
            assert!(adm.remaining() <= 3);
        }
        assert_eq!(adm.remaining(), 0);
    }

    #[test]
    fn admission_release_returns_quota() {
        let mut adm = Admission::new(8);
        let g = adm.admit(8).unwrap();
        assert_eq!(adm.admit(1), Err(ConnectErr::Refused));
        adm.release(g);
        assert_eq!(adm.remaining(), 8);
        // A second (erroneous) release does not underflow the accounting.
        adm.release(g);
        assert_eq!(adm.remaining(), 8);
    }

    #[test]
    fn admit_connect_decodes_admits_and_refuses() {
        let mut adm = Admission::new(4);
        let ok = admit_connect(&mut adm, &ConnectReq::for_window(4).encode());
        assert_eq!(ok, GrantReply::Grant(WindowGrant { window: 0, size: 4 }));
        // Quota now exhausted: a second connect is refused.
        let no = admit_connect(&mut adm, &ConnectReq::for_window(1).encode());
        assert_eq!(no, GrantReply::Refused);
        // A malformed request is refused, not granted.
        let bad = admit_connect(&mut adm, &[0xFF, 0xFF]);
        assert_eq!(bad, GrantReply::Refused);
    }
}
