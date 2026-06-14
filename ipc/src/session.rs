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

// Tag bytes for the fixed wire forms (first payload byte). Distinct so a
// decoder rejects a message of the wrong kind rather than misreading it.
const TAG_REQ: u8 = 0xC0;
const TAG_GRANT: u8 = 0x01;
const TAG_REFUSED: u8 = 0x00;

const REQ_LEN: usize = 5; // tag + u32
const GRANT_LEN: usize = 9; // tag + u32 window + u32 size
const REFUSED_LEN: usize = 1; // tag

impl ConnectReq {
    /// A request for a `requested` byte bulk window.
    pub const fn for_window(requested: u32) -> ConnectReq {
        ConnectReq { requested_window: requested }
    }

    pub fn encode(&self) -> [u8; REQ_LEN] {
        let mut b = [0u8; REQ_LEN];
        b[0] = TAG_REQ;
        b[1..5].copy_from_slice(&self.requested_window.to_le_bytes());
        b
    }

    /// Decode exactly `REQ_LEN` bytes; reject any other length or a bad tag
    /// (total over byte values, like [`crate::header::Header::decode`]).
    pub fn decode(buf: &[u8]) -> Option<ConnectReq> {
        if buf.len() != REQ_LEN || buf[0] != TAG_REQ {
            return None;
        }
        Some(ConnectReq {
            requested_window: u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]),
        })
    }
}

impl GrantReply {
    pub fn encode(&self) -> ([u8; GRANT_LEN], usize) {
        let mut b = [0u8; GRANT_LEN];
        match self {
            GrantReply::Grant(g) => {
                b[0] = TAG_GRANT;
                b[1..5].copy_from_slice(&g.window.to_le_bytes());
                b[5..9].copy_from_slice(&g.size.to_le_bytes());
                (b, GRANT_LEN)
            }
            GrantReply::Refused => {
                b[0] = TAG_REFUSED;
                (b, REFUSED_LEN)
            }
        }
    }

    /// Decode a reply: a `GRANT_LEN` grant or a `REFUSED_LEN` refusal; reject
    /// anything else.
    pub fn decode(buf: &[u8]) -> Option<GrantReply> {
        match buf.first().copied()? {
            TAG_GRANT if buf.len() == GRANT_LEN => Some(GrantReply::Grant(WindowGrant {
                window: u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]),
                size: u32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]),
            })),
            TAG_REFUSED if buf.len() == REFUSED_LEN => Some(GrantReply::Refused),
            _ => None,
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
    /// A quota that will grant at most `budget` window bytes in total.
    pub const fn new(budget: u32) -> Admission {
        Admission { budget, granted: 0 }
    }

    /// Window bytes still available to grant.
    pub const fn remaining(&self) -> u32 {
        self.budget - self.granted
    }

    /// The single admission decision (§3.5): grant `requested` bytes iff they fit
    /// the remaining quota, accounting for them; otherwise refuse and leave the
    /// quota untouched. Never grants past `budget` — the invariant
    /// `granted <= budget` holds after every call.
    pub fn admit(&mut self, requested: u32) -> Result<WindowGrant, ConnectErr> {
        if requested <= self.remaining() {
            self.granted += requested;
            Ok(WindowGrant { window: 0, size: requested })
        } else {
            Err(ConnectErr::Refused)
        }
    }

    /// Return a granted window's bytes to the quota on session close. A grant is
    /// released exactly once; releasing more than was granted is clamped to zero
    /// (defensive — a double release must not underflow the accounting).
    pub fn release(&mut self, grant: WindowGrant) {
        self.granted = self.granted.saturating_sub(grant.size);
    }
}

/// The server's connect step (§4.6), the admission point as a pure function:
/// decode the request bytes, decide under `adm`, and return the reply to send
/// back. A request that does not decode is refused (a server cannot grant a
/// window it cannot size). The caller does the transport round-trip — `recv_nb`
/// the request, `admit_connect`, `send_nb` the `encode`d reply — inside its
/// reactor loop, the same shape as serving any other request.
pub fn admit_connect(adm: &mut Admission, req_bytes: &[u8]) -> GrantReply {
    match ConnectReq::decode(req_bytes) {
        Some(req) => match adm.admit(req.requested_window) {
            Ok(g) => GrantReply::Grant(g),
            Err(_) => GrantReply::Refused,
        },
        None => GrantReply::Refused,
    }
}

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
