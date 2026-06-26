//! Sessions and connect (spec rev2§3.5) — the **admission** layer. A client
//! funds the channel (retypes it from its own untyped, rev2§3.2) and sends a
//! `ConnectReq` naming a requested **bulk-window size**; the server grants or
//! refuses at a **single admission point**, bounded by its per-server window
//! quota. Queue memory is the connector's; window memory is the server's, capped
//! by the quota it enforces here — so an anonymous connect cannot drain the
//! server (rev2§3.5, "fund by failure mode").
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
//! **Verified by Verus.** The codecs are total bijections (`[`req_encode`]`/
//! `[`req_decode`]`, `[`grant_encode`]`/`[`grant_decode`]` as ghost models, the
//! round-trip lemmas ∀); and [`Admission`] **never over-grants for all
//! admit/release sequences** — `granted <= budget` (`Admission::well_formed`)
//! is a pre/post-condition of every op, proven once and composed over *any*
//! sequence by Verus's modular reasoning, so `remaining()`'s `budget - granted`
//! is non-underflowing. As in `header.rs` the exec codecs use explicit
//! mask/shift arithmetic (not `to_le_bytes`/`copy_from_slice`, which Verus does
//! not spec), so `vstd` stays ghost-only and erases into the alloc-free user
//! binaries; the bytes produced are unchanged.
use vstd::prelude::*;

verus! {

// Tag bytes for the fixed wire forms (first payload byte). Distinct so a
// decoder rejects a message of the wrong kind rather than misreading it. `pub`
// so the `pub open spec` codec models below can name them (the `verus!{}`
// rule: an open spec body cannot reference a private item; cf. `header::HEADER_SIZE`).
pub const TAG_REQ: u8 = 0xC0;

pub const TAG_GRANT: u8 = 0x01;

pub const TAG_REFUSED: u8 = 0x00;

pub const REQ_LEN: usize = 7;

// tag + u32 window + u8 min_version + u8 max_version
pub const GRANT_LEN: usize = 10;

// tag + u32 window + u32 size + u8 version
pub const REFUSED_LEN: usize = 1;

// tag
/// The sole wire version this build of the connect layer offers today
/// (rev2§3.7). The connect layer negotiates a version even though exactly one is
/// deployed — the mechanism is fully present, so a future second version (and the
/// "old and new clients coexist per session" case, rev2§8.3) is a value change,
/// not a re-design. A server holds its own supported [`VersionRange`]; the MVP's
/// is the single point `[PROTOCOL_VERSION, PROTOCOL_VERSION]`.
pub const PROTOCOL_VERSION: u8 = 1;

/// A contiguous span of supported wire versions, `[min, max]` inclusive
/// (rev2§3.7). A client advertises its range in the [`ConnectReq`]; the server
/// holds its own range and selects the highest common version via [`negotiate`].
/// A contiguous range matches the monotone "a breaking change is a new version
/// number" framing (rev2§3.7) and the "old and new clients coexist" case
/// (rev2§8.3); a non-contiguous version *bitset* is the recorded forward,
/// append-only generalization, not built now (one version is deployed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRange {
    pub min: u8,
    pub max: u8,
}

impl VersionRange {
    /// A range spanning `[min, max]`.
    pub fn new(min: u8, max: u8) -> (r: VersionRange)
        ensures
            r.min == min,
            r.max == max,
    {
        VersionRange { min, max }
    }

    /// The degenerate range offering exactly one version — today's MVP case.
    pub fn single(v: u8) -> (r: VersionRange)
        ensures
            r.min == v,
            r.max == v,
    {
        VersionRange { min: v, max: v }
    }
}

/// A granted bulk window (rev2§3.1): which window and how many bytes. The MVP
/// grants a single window, so `window` is always 0; it exists so the descriptor
/// ABI is grow-only when multi-window lands (out of scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGrant {
    pub window: u32,
    pub size: u32,
}

/// A connect request: the client's requested bulk-window size in bytes, plus the
/// contiguous range of wire versions it speaks (rev2§3.7). The server selects the
/// highest common version at the single admission point ([`negotiate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectReq {
    pub requested_window: u32,
    pub versions: VersionRange,
}

/// The server's reply to a connect: a granted window **and the negotiated wire
/// version**, or a refusal (the single admission point). A refusal covers both a
/// window that does not fit the quota and a version with no overlap — the wire
/// form does not distinguish them (a refusal is a refusal); the server-internal
/// [`ConnectErr`] does, for diagnosability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantReply {
    Grant(WindowGrant, u8),
    Refused,
}

/// Why a connect failed (server-internal; the wire [`GrantReply`] collapses both
/// to `Refused`). The client-side connect *mechanism* (the endpoint-cap
/// handshake, rev2§3.5) is deferred, so its richer errors — a peer-closed session
/// channel, a reply that does not decode, a transport error — are not yet
/// constructed. They return when that mechanism lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectErr {
    /// The server refused under its window quota (rev2§3.5).
    Refused,
    /// No wire version is common to the client's offered range and the server's
    /// supported range (rev2§3.7). Distinguished from `Refused` so a future
    /// client can know not to retry a version refusal.
    VersionMismatch,
}

// ── Ghost models of the codecs (the little-endian byte layout as a `Seq`) ──
/// Ghost model of [`ConnectReq::encode`]: tag byte, the requested-window `u32`
/// split low-to-high (matching `to_le_bytes`), then the offered version range as
/// two raw bytes (`min`, `max`). The version bytes sit at the end, so the
/// tag + requested-window prefix is independent of the version range.
pub open spec fn req_encode(r: ConnectReq) -> Seq<u8> {
    seq![
        TAG_REQ,
        (r.requested_window & 0xff) as u8,
        ((r.requested_window >> 8) & 0xff) as u8,
        ((r.requested_window >> 16) & 0xff) as u8,
        ((r.requested_window >> 24) & 0xff) as u8,
        r.versions.min,
        r.versions.max,
    ]
}

/// Ghost model of [`ConnectReq::decode`]: `Some` iff exactly `REQ_LEN` bytes
/// tagged `TAG_REQ`, reassembling the little-endian `u32` and the two raw version
/// bytes; `None` otherwise. Total over every byte string.
pub open spec fn req_decode(s: Seq<u8>) -> Option<ConnectReq> {
    if s.len() == REQ_LEN && s[0] == TAG_REQ {
        Some(
            ConnectReq {
                requested_window: (s[1] as u32) | ((s[2] as u32) << 8) | ((s[3] as u32) << 16) | ((
                s[4] as u32) << 24),
                versions: VersionRange { min: s[5], max: s[6] },
            },
        )
    } else {
        None
    }
}

/// Ghost model of [`GrantReply::encode`]'s *used prefix*: a `GRANT_LEN` grant
/// (tag + window + size, each little-endian, then the negotiated version byte) or
/// a `REFUSED_LEN` refusal (tag). The version byte sits at the end, after the
/// window+size fields.
pub open spec fn grant_encode(g: GrantReply) -> Seq<u8> {
    match g {
        GrantReply::Grant(w, ver) => seq![
            TAG_GRANT,
            (w.window & 0xff) as u8,
            ((w.window >> 8) & 0xff) as u8,
            ((w.window >> 16) & 0xff) as u8,
            ((w.window >> 24) & 0xff) as u8,
            (w.size & 0xff) as u8,
            ((w.size >> 8) & 0xff) as u8,
            ((w.size >> 16) & 0xff) as u8,
            ((w.size >> 24) & 0xff) as u8,
            ver,
        ],
        GrantReply::Refused => seq![TAG_REFUSED],
    }
}

/// Ghost model of [`GrantReply::decode`]: a `GRANT_LEN` grant (tag `TAG_GRANT`,
/// window + size + version byte) or a `REFUSED_LEN` refusal (tag `TAG_REFUSED`);
/// `None` otherwise. Total.
pub open spec fn grant_decode(s: Seq<u8>) -> Option<GrantReply> {
    if s.len() == GRANT_LEN && s[0] == TAG_GRANT {
        Some(
            GrantReply::Grant(
                WindowGrant {
                    window: (s[1] as u32) | ((s[2] as u32) << 8) | ((s[3] as u32) << 16) | ((
                    s[4] as u32) << 24),
                    size: (s[5] as u32) | ((s[6] as u32) << 8) | ((s[7] as u32) << 16) | ((
                    s[8] as u32) << 24),
                },
                s[9],
            ),
        )
    } else if s.len() == REFUSED_LEN && s[0] == TAG_REFUSED {
        Some(GrantReply::Refused)
    } else {
        None
    }
}

impl ConnectReq {
    /// A request for a `requested` byte bulk window, offering the single wire
    /// version this build speaks ([`PROTOCOL_VERSION`]) — the MVP client.
    pub fn for_window(requested: u32) -> (r: ConnectReq)
        ensures
            r.requested_window == requested,
            r.versions.min == PROTOCOL_VERSION,
            r.versions.max == PROTOCOL_VERSION,
    {
        ConnectReq { requested_window: requested, versions: VersionRange::single(PROTOCOL_VERSION) }
    }

    /// A request for a `requested` byte bulk window offering `versions`.
    pub fn new(requested: u32, versions: VersionRange) -> (r: ConnectReq)
        ensures
            r.requested_window == requested,
            r.versions == versions,
    {
        ConnectReq { requested_window: requested, versions }
    }

    pub fn encode(&self) -> (b: [u8; REQ_LEN])
        ensures
            b@ == req_encode(*self),
    {
        broadcast use vstd::array::group_array_axioms;

        let b: [u8; REQ_LEN] = [
            TAG_REQ,
            (self.requested_window & 0xff) as u8,
            ((self.requested_window >> 8) & 0xff) as u8,
            ((self.requested_window >> 16) & 0xff) as u8,
            ((self.requested_window >> 24) & 0xff) as u8,
            self.versions.min,
            self.versions.max,
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
        Some(
            ConnectReq {
                requested_window: (buf[1] as u32) | ((buf[2] as u32) << 8) | ((buf[3] as u32) << 16)
                    | ((buf[4] as u32) << 24),
                versions: VersionRange { min: buf[5], max: buf[6] },
            },
        )
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
            GrantReply::Grant(g, ver) => {
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
                    ver,
                ];
                assert(b@.subrange(0, GRANT_LEN as int) =~= grant_encode(*self));
                (b, GRANT_LEN)
            },
            GrantReply::Refused => {
                let b: [u8; GRANT_LEN] = [TAG_REFUSED, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                assert(b@.subrange(0, REFUSED_LEN as int) =~= grant_encode(*self));
                (b, REFUSED_LEN)
            },
        }
    }

    /// Decode a reply: a `GRANT_LEN` grant or a `REFUSED_LEN` refusal; reject
    /// anything else (total over byte values).
    pub fn decode(buf: &[u8]) -> (r: Option<GrantReply>)
        ensures
            r == grant_decode(buf@),
            r is Some <==> ((buf@.len() == GRANT_LEN && buf@[0] == TAG_GRANT) || (buf@.len()
                == REFUSED_LEN && buf@[0] == TAG_REFUSED)),
    {
        broadcast use vstd::slice::group_slice_axioms;

        if buf.len() == GRANT_LEN && buf[0] == TAG_GRANT {
            Some(
                GrantReply::Grant(
                    WindowGrant {
                        window: (buf[1] as u32) | ((buf[2] as u32) << 8) | ((buf[3] as u32) << 16)
                            | ((buf[4] as u32) << 24),
                        size: (buf[5] as u32) | ((buf[6] as u32) << 8) | ((buf[7] as u32) << 16) | (
                        (buf[8] as u32) << 24),
                    },
                    buf[9],
                ),
            )
        } else if buf.len() == REFUSED_LEN && buf[0] == TAG_REFUSED {
            Some(GrantReply::Refused)
        } else {
            None
        }
    }
}

/// The per-server bulk-window quota (rev2§3.5): the **single admission point**.
/// Tracks a fixed `budget` of window bytes and how much is currently `granted`;
/// `admit` hands out a window iff it fits the remainder (it **never** over-grants
/// — the quota invariant a malicious flood of connects cannot break), and
/// `release` returns the bytes when a session closes. The MVP grants one window
/// per session, all into window 0.
///
/// This is the verified-accounting template of `doc/guidelines/verus.md` §14 (a
/// `well_formed` cap + a non-underflowing observable preserved by every mutator),
/// which the reactor `used`-mask dispatch accounting reuses.
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
        requires
            self.well_formed(),
        ensures
            r == self.spec_remaining(),
    {
        self.budget - self.granted
    }

    /// The single admission decision (rev2§3.5): grant `requested` bytes iff they fit
    /// the remaining quota, accounting for them; otherwise refuse and leave the
    /// quota untouched. **Never grants past budget** — `Admission::well_formed`
    /// holds after every call, for *any* `requested`, so a flood of connects can
    /// never push `granted` past `budget` (the unbounded never-over-grant theorem).
    pub fn admit(&mut self, requested: u32) -> (res: Result<WindowGrant, ConnectErr>)
        requires
            self.well_formed(),
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
        requires
            self.well_formed(),
        ensures
            final(self).well_formed(),
            final(self).spec_remaining() >= old(self).spec_remaining(),
    {
        self.granted = self.granted.saturating_sub(grant.size);
    }
}

/// `v` is a wire version both `client` and `server` speak — it lies in both
/// `[min, max]` ranges. The common versions form the intersection; [`negotiate`]
/// returns the greatest of them (the highest both can speak), or `None` when it
/// is empty.
pub open spec fn common(client: VersionRange, server: VersionRange, v: u8) -> bool {
    client.min <= v <= client.max && server.min <= v <= server.max
}

/// The version selection (rev2§3.7, *"versions are negotiated once at session
/// establishment … a server may speak several concurrently"*): the **highest
/// common** version of the client's offered range and the server's supported
/// range, or `None` when the ranges are disjoint (a clean refusal, not a crash).
/// Pure and total over arbitrary `u8` ranges — a malformed client range (`min >
/// max`, possible from decoded bytes) denotes an empty set and yields `None`.
pub fn negotiate(client: VersionRange, server: VersionRange) -> (r: Option<u8>)
    ensures
        match r {
            Some(v) => common(client, server, v) && (forall|w: u8|
                common(client, server, w) ==> w <= v),
            None => forall|w: u8| !common(client, server, w),
        },
{
    // lo/hi bound the intersection: lo = max of the mins, hi = min of the maxes.
    // Non-empty iff lo <= hi, and then hi is the highest common version.
    let lo = if client.min >= server.min {
        client.min
    } else {
        server.min
    };
    let hi = if client.max <= server.max {
        client.max
    } else {
        server.max
    };
    if lo <= hi {
        Some(hi)
    } else {
        None
    }
}

/// Per-message version validation (rev2§2.7/§3.7): a message's stamped header
/// version must equal the session's negotiated version, else the message is
/// refused — *never* a crash. This is dispatch-discipline outside the header
/// codec, so the header layout and its bijection proofs are untouched (rev2§3.7).
/// Inert until the dispatch site is wired.
pub fn version_ok(header_version: u8, negotiated: u8) -> (ok: bool)
    ensures
        ok == (header_version == negotiated),
{
    header_version == negotiated
}

/// The server's connect step, the admission point as a pure function:
/// decode the request bytes, **negotiate the wire version** against the server's
/// supported `server` range, decide the window under `adm`, and return the reply
/// to send back. A request that does not decode, names no common version, or asks
/// for a window past the quota is refused (the wire `GrantReply::Refused` covers
/// all three — the server-internal [`ConnectErr`] distinguishes them). The caller
/// does the transport round-trip — `recv_nb` the request, `admit_connect`,
/// `send_nb` the `encode`d reply — inside its reactor loop, the same shape as
/// serving any other request. Preserves the quota invariant.
pub fn admit_connect(adm: &mut Admission, server: VersionRange, req_bytes: &[u8]) -> (r: GrantReply)
    requires
        adm.well_formed(),
    ensures
        final(adm).well_formed(),
{
    // Decode → negotiate version → admit window. Each failure is an internal
    // `ConnectErr` (for diagnosability) collapsed to the single wire refusal.
    let decided: Result<(WindowGrant, u8), ConnectErr> = match ConnectReq::decode(req_bytes) {
        Some(req) => match negotiate(req.versions, server) {
            Some(ver) => match adm.admit(req.requested_window) {
                Ok(g) => Ok((g, ver)),
                Err(e) => Err(e),
            },
            None => Err(ConnectErr::VersionMismatch),
        },
        None => Err(ConnectErr::Refused),
    };
    match decided {
        Ok((g, ver)) => GrantReply::Grant(g, ver),
        Err(_) => GrantReply::Refused,
    }
}

// ── Codec bijection lemmas (∀; the bit_vector split/reassemble identities) ──
/// `decode`∘`encode` is the identity on `ConnectReq`: every request round-trips.
/// The `u32` window reassembles by the same `bit_vector` split identity as
/// before; the two version bytes are carried verbatim, so they need no reasoning.
pub proof fn lemma_req_decode_encode(r: ConnectReq)
    ensures
        req_decode(req_encode(r)) == Some(r),
{
    let s = req_encode(r);
    assert(s.len() == REQ_LEN);
    let rw = r.requested_window;
    crate::le_bytes::lemma_u32_le_reassemble(rw);
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
    let s1 = s[1];
    let s2 = s[2];
    let s3 = s[3];
    let s4 = s[4];
    crate::le_bytes::lemma_u32_le_split_bytes(s1, s2, s3, s4);
    // s[5], s[6] (the version bytes) are reproduced directly; extensionality closes it.
    assert(req_encode(req_decode(s)->Some_0) =~= s);
}

/// `decode`∘`encode` is the identity on `GrantReply` (both arms). The window/size
/// `u32`s reassemble by the same `bit_vector` identities; the appended version
/// byte is carried verbatim.
pub proof fn lemma_grant_decode_encode(g: GrantReply)
    ensures
        grant_decode(grant_encode(g)) == Some(g),
{
    match g {
        GrantReply::Grant(w, _ver) => {
            let win = w.window;
            let sz = w.size;
            crate::le_bytes::lemma_u32_le_reassemble(win);
            crate::le_bytes::lemma_u32_le_reassemble(sz);
        },
        GrantReply::Refused => {},
    }
}

/// `encode`∘`decode` is the identity on accepted reply bytes (grant + refusal).
/// With [`lemma_grant_decode_encode`] the reply codec is a total bijection
/// between `GrantReply` values and their accepted byte strings.
pub proof fn lemma_grant_encode_decode(s: Seq<u8>)
    requires
        (s.len() == GRANT_LEN && s[0] == TAG_GRANT) || (s.len() == REFUSED_LEN && s[0]
            == TAG_REFUSED),
    ensures
        grant_encode(grant_decode(s)->Some_0) == s,
{
    if s.len() == GRANT_LEN && s[0] == TAG_GRANT {
        let s1 = s[1];
        let s2 = s[2];
        let s3 = s[3];
        let s4 = s[4];
        let s5 = s[5];
        let s6 = s[6];
        let s7 = s[7];
        let s8 = s[8];
        crate::le_bytes::lemma_u32_le_split_bytes(s1, s2, s3, s4);
        crate::le_bytes::lemma_u32_le_split_bytes(s5, s6, s7, s8);
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
        // A multi-version offer round-trips too (the version bytes are carried).
        let r2 = ConnectReq::new(8192, VersionRange::new(1, 4));
        assert_eq!(ConnectReq::decode(&r2.encode()), Some(r2));
    }

    #[test]
    fn connect_req_rejects_bad_len_and_tag() {
        assert_eq!(ConnectReq::decode(&[]), None);
        assert_eq!(ConnectReq::decode(&[TAG_REQ, 0, 0, 0, 0, 0]), None); // short (REQ_LEN-1)
        assert_eq!(ConnectReq::decode(&[TAG_REQ, 0, 0, 0, 0, 0, 0, 0]), None); // trailing (REQ_LEN+1)
        assert_eq!(ConnectReq::decode(&[0xFF, 0, 0, 0, 0, 0, 0]), None); // right len, wrong tag
    }

    #[test]
    fn grant_reply_roundtrip() {
        let g = GrantReply::Grant(
            WindowGrant {
                window: 0,
                size: 8192,
            },
            3,
        );
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
    fn negotiate_single_version() {
        // The deployed MVP case: one version offered on both sides.
        assert_eq!(
            negotiate(VersionRange::single(1), VersionRange::single(1)),
            Some(1)
        );
    }

    #[test]
    fn negotiate_disjoint_refuses() {
        assert_eq!(
            negotiate(VersionRange::single(1), VersionRange::single(2)),
            None
        );
        assert_eq!(
            negotiate(VersionRange::new(5, 9), VersionRange::new(1, 3)),
            None
        );
        // Adjacent but non-overlapping ([1,2] and [3,4]) share no version.
        assert_eq!(
            negotiate(VersionRange::new(1, 2), VersionRange::new(3, 4)),
            None
        );
    }

    #[test]
    fn negotiate_picks_highest_common() {
        // Overlap: highest common is min(client.max, server.max).
        assert_eq!(
            negotiate(VersionRange::new(1, 4), VersionRange::new(3, 6)),
            Some(4)
        );
        // Nested — client inside server.
        assert_eq!(
            negotiate(VersionRange::new(3, 3), VersionRange::new(1, 9)),
            Some(3)
        );
        // Nested — server inside client.
        assert_eq!(
            negotiate(VersionRange::new(1, 5), VersionRange::new(2, 3)),
            Some(3)
        );
        // Touching at exactly one version.
        assert_eq!(
            negotiate(VersionRange::new(1, 3), VersionRange::new(3, 5)),
            Some(3)
        );
    }

    #[test]
    fn negotiate_malformed_range_refuses() {
        // A client range with min > max denotes an empty set → no common version.
        assert_eq!(
            negotiate(VersionRange::new(5, 2), VersionRange::new(1, 9)),
            None
        );
    }

    #[test]
    fn version_ok_matches_exactly() {
        assert!(version_ok(2, 2));
        assert!(!version_ok(1, 2));
        assert!(!version_ok(2, 1));
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
    fn admit_connect_negotiates_admits_and_refuses() {
        let server = VersionRange::single(PROTOCOL_VERSION);
        let mut adm = Admission::new(4);
        // A matching version + a window that fits → granted at the negotiated version.
        let ok = admit_connect(&mut adm, server, &ConnectReq::for_window(4).encode());
        assert_eq!(
            ok,
            GrantReply::Grant(WindowGrant { window: 0, size: 4 }, PROTOCOL_VERSION)
        );
        // Quota now exhausted: a second (version-matching) connect is refused.
        let no = admit_connect(&mut adm, server, &ConnectReq::for_window(1).encode());
        assert_eq!(no, GrantReply::Refused);
        // A malformed request is refused, not granted.
        let bad = admit_connect(&mut adm, server, &[0xFF, 0xFF]);
        assert_eq!(bad, GrantReply::Refused);
    }

    #[test]
    fn admit_connect_refuses_version_mismatch_without_touching_quota() {
        let server = VersionRange::single(PROTOCOL_VERSION);
        let mut adm = Admission::new(64);
        // A version with no overlap is refused even though the window would fit,
        // and the quota is left untouched (admit is never reached).
        let mismatch = admit_connect(
            &mut adm,
            server,
            &ConnectReq::new(4, VersionRange::single(PROTOCOL_VERSION.wrapping_add(9))).encode(),
        );
        assert_eq!(mismatch, GrantReply::Refused);
        assert_eq!(adm.remaining(), 64);
    }

    #[test]
    fn admit_connect_selects_highest_common_version() {
        // Server speaks [1,3]; a client offering [2,5] is granted at version 3.
        let mut adm = Admission::new(8);
        let server = VersionRange::new(1, 3);
        let reply = admit_connect(
            &mut adm,
            server,
            &ConnectReq::new(8, VersionRange::new(2, 5)).encode(),
        );
        assert_eq!(
            reply,
            GrantReply::Grant(WindowGrant { window: 0, size: 8 }, 3)
        );
    }
}

// The negotiation/admission property tier (rev2§6, *"everything gets
// Miri+proptest"*). Pure, sequential decision logic — no concurrency — so plain
// proptest is the tool; the `not(loom)/not(shuttle)` gate mirrors `reactor.rs`
// (those harnesses drive the concurrent shape, not this).
#[cfg(all(test, not(loom), not(shuttle)))]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    // ── Independent exec oracles ──
    // The Verus `ensures` on `negotiate` is stated against the *ghost* `common`
    // spec fn, which is not callable from exec/test code. These plain-Rust
    // oracles re-derive the same notion by brute force over the whole u8 domain,
    // independent of `negotiate`'s lo/hi arithmetic — so checking agreement is a
    // real test, not a tautology (the project's independent-oracle posture, cf.
    // `loader/tests/layout_props.rs`).

    /// Does `v` lie in both ranges? (the exec twin of the ghost `common`.)
    fn is_common(client: VersionRange, server: VersionRange, v: u8) -> bool {
        client.min <= v && v <= client.max && server.min <= v && v <= server.max
    }

    /// The highest version both ranges contain, by a brute-force scan from the
    /// top — `None` when the intersection is empty. Independent of `negotiate`.
    fn highest_common(client: VersionRange, server: VersionRange) -> Option<u8> {
        (0u8..=u8::MAX)
            .rev()
            .find(|&v| is_common(client, server, v))
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            failure_persistence: if cfg!(miri) { None } else { ProptestConfig::default().failure_persistence },
            .. ProptestConfig::default()
        })]

        /// `negotiate` returns **exactly** the highest common version of the two
        /// ranges, for *every* pair — overlapping, nested, touching, disjoint, and
        /// malformed (`min > max`, reachable from decoded bytes). Equality against
        /// the independent oracle pins the *selection*, not merely "a common
        /// version." The selection is also symmetric in the two ranges (it is a
        /// property of the intersection, not of argument order).
        #[test]
        fn negotiate_is_highest_common(
            c_min in any::<u8>(),
            c_max in any::<u8>(),
            s_min in any::<u8>(),
            s_max in any::<u8>(),
        ) {
            let client = VersionRange::new(c_min, c_max);
            let server = VersionRange::new(s_min, s_max);
            prop_assert_eq!(negotiate(client, server), highest_common(client, server));
            prop_assert_eq!(negotiate(client, server), negotiate(server, client));
        }

        /// Over an arbitrary sequence of connects against one server range and one
        /// quota: `admit_connect` grants **at the negotiated version** iff a common
        /// version exists *and* the window fits the remaining quota, else refuses;
        /// and it **never over-grants** — `remaining()` tracks `budget − Σgranted`
        /// exactly and never exceeds the budget. The runtime witness, over whole
        /// sequences, for the Verus `Admission` never-over-grant invariant.
        #[test]
        fn admit_connect_sequence_grants_and_never_over_grants(
            steps in prop::collection::vec((0u32..=300, any::<u8>(), any::<u8>()), 0..40),
        ) {
            const BUDGET: u32 = 1000;
            let server = VersionRange::new(2, 4);
            let mut adm = Admission::new(BUDGET);
            let mut remaining: u32 = BUDGET; // the oracle

            for (window, c_min, c_max) in steps {
                let vrange = VersionRange::new(c_min, c_max);
                let reply = admit_connect(&mut adm, server, &ConnectReq::new(window, vrange).encode());

                match (highest_common(vrange, server), window <= remaining) {
                    (Some(ver), true) => {
                        // Common version + fitting window: granted at that version.
                        prop_assert_eq!(
                            reply,
                            GrantReply::Grant(WindowGrant { window: 0, size: window }, ver)
                        );
                        remaining -= window;
                    }
                    _ => {
                        // No common version, or the window does not fit: refused,
                        // and the quota is left untouched.
                        prop_assert_eq!(reply, GrantReply::Refused);
                    }
                }
                // Never over-grant: the accounting matches the oracle and stays in [0, BUDGET].
                prop_assert_eq!(adm.remaining(), remaining);
                prop_assert!(remaining <= BUDGET);
            }
        }
    }

    // ── Negative control (the anti-theater check) ──

    /// A deliberately-wrong selector: returns the client's max while ignoring
    /// whether the *server* can speak it — exactly the bug `negotiate` exists to
    /// prevent (a client forcing a version the server does not support). Used
    /// only by the control below to show the proptest above has teeth.
    fn bad_negotiate(client: VersionRange, _server: VersionRange) -> Option<u8> {
        Some(client.max)
    }

    /// Negative control (same posture as `loader`'s
    /// `old_unchecked_formula_would_wrap_on_i5_witness`): on inputs where the
    /// server cannot speak the client's max, `bad_negotiate` disagrees with the
    /// brute-force oracle and selects a **non-common** version — so substituting
    /// it for `negotiate` would make `negotiate_is_highest_common` fail. This
    /// proves the property is not vacuously satisfiable.
    #[test]
    fn negotiate_negative_control_has_teeth() {
        // Disjoint ranges: no common version at all, yet the wrong oracle still
        // returns the client's max.
        let client = VersionRange::new(4, 5);
        let server = VersionRange::new(1, 2);
        assert_eq!(negotiate(client, server), None);
        assert_eq!(negotiate(client, server), highest_common(client, server)); // real: agree
        let bad = bad_negotiate(client, server).unwrap();
        assert_ne!(
            bad_negotiate(client, server),
            highest_common(client, server),
            "wrong oracle must violate the proptest property"
        );
        assert!(
            !is_common(client, server, bad),
            "wrong oracle picked a non-common version"
        );

        // Overlapping, but the wrong oracle still overshoots: the server caps at 3
        // while the client's max is 5.
        let client = VersionRange::new(1, 5);
        let server = VersionRange::new(1, 3);
        assert_eq!(negotiate(client, server), Some(3)); // real: highest common
        let bad = bad_negotiate(client, server).unwrap();
        assert_eq!(bad, 5);
        assert_ne!(
            bad_negotiate(client, server),
            highest_common(client, server),
            "wrong oracle must violate the proptest property"
        );
        assert!(
            !is_common(client, server, bad),
            "the server cannot speak version 5"
        );
    }
}
