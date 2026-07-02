// SPDX-License-Identifier: 0BSD
//! The startup block (rev2§5.1): the first message on a child's bootstrap
//! channel, carrying *argv*, *env*, and a **named-grant table**. One versioned,
//! self-describing format (`b"EUS1"`) serves every parent→child bootstrap
//! (init→storaged, init→shell, shell→child).
//!
//! Strict, like the sibling `elf` decoder: the block is **untrusted-shaped
//! input** consumed in `_start` before anything else exists, so `decode` is
//! **total over arbitrary bytes** — a malformed block is refused (`None`),
//! never a panic / out-of-bounds read / unbounded allocation (rev2§2.7
//! refuse-not-crash). That totality is **mechanized in Verus** (the `elf::parse`
//! tier): `decode` carries a total ∀-bytes contract — on `Some`, the counts are
//! within their arenas and every borrowed argv/env byte-string is a subrange of
//! the input buffer (`well_formed_startup`). The producer side is total the
//! other way: `encode` refuses an over-budget block with a clean `Err`, never a
//! panic or a silent truncation. `no_std`/`core`-only (no `alloc`): argv/env
//! decode as borrowed slices into the message buffer, and the grant table lives
//! in a fixed-size arena, so a `no_std` `_start` reads a block without touching
//! a heap.
//!
//! ## Wire layout
//!
//! ```text
//! Startup block  (≤ MAX_BLOCK = 256 bytes, one bootstrap-channel message)
//!   Header (7 bytes):
//!     [0..4]  magic   : [u8;4] = b"EUS1"
//!     [4]     ngrants : u8
//!     [5]     nargv   : u8
//!     [6]     nenv    : u8
//!   Grants (ngrants entries; each tagged, so its size is a function of kind):
//!     name : u8     well-known name id (NAME_*)
//!     kind : u8     KIND_CAP_SLOT | KIND_STORAGE_HANDLE | KIND_REGION | KIND_SEED
//!       KIND_CAP_SLOT       : slot:u32                  (entry = 6 bytes)
//!       KIND_STORAGE_HANDLE : handle:u32                (entry = 6 bytes)
//!       KIND_REGION         : va:u64, len:u64, pa:u64   (entry = 26 bytes)
//!       KIND_SEED           : seed:[u64;4]              (entry = 34 bytes)
//!   Argv (nargv entries):  each  len:u16, then len bytes
//!   Env  (nenv  entries):  each  len:u16, then len bytes
//! ```
//!
//! The grant kinds: `CAP_SLOT` and `STORAGE_HANDLE` are the spec's literal two
//! (kernel caps resolve to cspace slots, storage grants resolve to handle
//! numbers). `REGION` generalizes the one grant the system already delivers —
//! `time` is a pre-mapped VA in every block, and an MMIO/DMA grant has the
//! same shape (VA + len, plus a device PA for DMA). A region carries **no new
//! authority**: the parent maps the page before start; only the VA travels.
//! `SEED` is the sole *inline-bytes* kind: it carries 256 bits of entropy by
//! value (the `NAME_RANDOM_SEED` grant, rev2§5.1) — the parent's per-child
//! sub-seed for the process DRBG. Unlike the others it holds
//! *owned* bytes, not a reference to a cap or a mapped page, so the decoder
//! copies the four words out of the message and borrows nothing new.
// `vstd::prelude` supplies the `verus!{}` macro + ghost vocabulary for the
// startup decoder's total bounded-decoder proof (below); Verus requires it
// imported at the crate root, and in an ordinary build the macro erases ghost
// code, so the import is otherwise unused — hence the allow (same as elf.rs /
// freelist / virtio-blk).
#[allow(unused_imports)]
use vstd::prelude::*;

/// Magic for the unified startup block, version 1.
pub const MAGIC: [u8; 4] = *b"EUS1";

/// Hard size budget for one block: the kernel's `MSG_PAYLOAD`
/// (`kcore::channel::MSG_PAYLOAD = 256`). A block that would exceed this is
/// refused by `encode`, since it could not be delivered in one message.
pub const MAX_BLOCK: usize = 256;

// Well-known name ids (rev2§5.1 standard names + bring-up device names). A
// small `u8` enum so a `no_std` `_start` resolves a name with an integer match,
// not string handling. `name = 0` is reserved as a future string-name escape so
// the eventual stable public ABI (rev2§8.3) can widen to byte-string names
// without a format break; v1 uses ids only.
/// Reserved string-name escape (unused in v1; see module docs).
pub const NAME_STRING: u8 = 0;
/// The process's storage root (rev2§5.1).
pub const NAME_ROOT: u8 = 1;
/// Standard input — a console-channel endpoint (rev2§5.1), deliberately split
/// from `stdout` so a pipeline wires one process's `stdout` to another's `stdin`.
/// An interactive console is one channel granted under both names.
pub const NAME_STDIN: u8 = 2;
/// Standard output — a console-channel endpoint, deliberately split from `stdin`.
pub const NAME_STDOUT: u8 = 3;
/// A writable scratch subtree (rev2§5.1). Reserved unless carvable.
pub const NAME_TMP: u8 = 4;
/// The process's storage session channel (rev2§5.1).
pub const NAME_STORAGE: u8 = 5;
/// The monotonic time page (rev2§2.6). The one named grant delivered today.
pub const NAME_TIME: u8 = 6;
// In-process-threading self-caps (scoped/opt-in). A thread-capable
// process holds caps to its own aspace (WRITE, to map thread stacks), its own
// cspace (to name in `thread_start_as`), and a thread-untyped to retype the
// per-thread objects from, plus the base of a reserved free cspace-slot range.
// All `CapSlot` grants — no codec/verified-decoder change (the same posture as
// the `stdin`/`stdout`/`stderr` console slots). Absent for a non-thread-capable
// process (least-authority default).
/// The process's own aspace cap slot (rev2§5.3).
pub const NAME_SELF_ASPACE: u8 = 7;
/// The process's own cspace cap slot (rev2§5.3).
pub const NAME_SELF_CSPACE: u8 = 8;
/// The untyped the process retypes thread objects from (rev2§5.3).
pub const NAME_THREAD_UNTYPED: u8 = 9;
/// The base of the reserved free cspace-slot range for per-thread caps; the count
/// is the fixed `urt::thread_layout::WORKING_SLOTS` convention (rev2§5.3).
pub const NAME_THREAD_SLOT_BASE: u8 = 10;
/// The process's per-run entropy seed: 256 bits the parent drew from its own DRBG
/// (rev2§5.1). A `KIND_SEED` inline-bytes grant — the sole grant
/// carrying an owned value rather than a cap/handle/region reference. The child
/// seeds its process DRBG (`urt::random`) from it; absent ⇒ `fill_bytes` loudly
/// aborts at first use (the `NAME_TIME` posture), never silently predictable.
pub const NAME_RANDOM_SEED: u8 = 11;
/// Standard error — a console-channel endpoint (rev2§5.1), a stream
/// distinct from `stdout` so diagnostics never enter a pipeline's data path. A
/// consumer resolves it as `NAME_STDERR` → else the `stdout` channel → else the
/// kernel debug-log. A `CapSlot` grant like `stdin`/`stdout`, decoded through the
/// existing `KIND_CAP_SLOT` arm — no codec/verified-decoder change.
pub const NAME_STDERR: u8 = 12;
/// The virtio MMIO transport window (bring-up; storaged).
pub const NAME_VIRTIO_MMIO: u8 = 16;
/// The DMA pool region (bring-up; storaged).
pub const NAME_DMA: u8 = 17;
/// The PL011 UART MMIO window (bring-up; the console driver). The driver
/// reads its register base from this `REGION` grant's VA rather than the
/// hardcoded 0x0900_0000, exactly as storaged reads `NAME_VIRTIO_MMIO`.
pub const NAME_PL011_MMIO: u8 = 18;

// The startup-block decoder is the Verus-verified deductive core, the twin of
// the `elf::parse` decoder: the arena-cap and grant-kind constants, the
// `GrantKind`/`Grant`/`Startup` types, the `well_formed_startup`/`subseq_of`
// predicates, the bounds-checked cursor helpers (`take_*`, reading fixed-width
// little-endian fields through the shared `le-bytes` crate's verified
// `read_u*_le` readers), and `decode`'s total bounded-decoder contract all live
// in the `verus!{}` block so the proofs can name them (doc/guidelines/verus.md
// §6). The startup *encoder* (`encode`/`Writer`), the rich `Startup` builder API
// (`new`/`push_*`/`grant`, the prefix-comparing `PartialEq`), and `EncodeError`
// stay external plain Rust — after erasure these are ordinary items, so the
// callers of `decode` keep working unchanged.
verus! {

/// Maximum grant-table entries a block may carry. The fixed arena `decode`
/// fills and the bound it validates `ngrants` against. Comfortably above the
/// real blocks (storaged's is 3; the shell's is ≤ 4).
pub const MAX_GRANTS: usize = 8;

/// Maximum argv byte-strings a block may carry.
pub const MAX_ARGV: usize = 8;

/// Maximum env byte-strings a block may carry.
pub const MAX_ENV: usize = 8;

/// Grant kind: a kernel cap, named by the cspace slot it was installed into.
pub const KIND_CAP_SLOT: u8 = 1;

/// Grant kind: a storage grant, named by its handle number on the session.
pub const KIND_STORAGE_HANDLE: u8 = 2;

/// Grant kind: a pre-mapped region (VA, length, and an optional device PA).
pub const KIND_REGION: u8 = 3;

/// Grant kind: 256 bits of entropy inline (the `NAME_RANDOM_SEED` seed).
pub const KIND_SEED: u8 = 4;

/// What a named grant resolves to. `CapSlot`/`StorageHandle` are the spec's two
/// kinds; `Region` is the additive pre-mapped-region kind (carries no new
/// authority — only a VA the parent already mapped); `Seed` is the additive
/// inline-bytes kind (carries an owned 256-bit value, no reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// A kernel cap, installed at this cspace slot index before start.
    CapSlot(u32),
    /// A storage grant, this handle number on the process's session channel.
    StorageHandle(u32),
    /// A pre-mapped region: virtual address, length, and device physical
    /// address (`pa == 0` unless it is a DMA region read through a phys-cap).
    Region { va: u64, len: u64, pa: u64 },
    /// An inline entropy seed: 256 bits (four LE `u64` words) the parent drew
    /// from its own DRBG for this child (rev2§5.1). Owned by value — nothing is
    /// borrowed out of the message — so `well_formed_startup` needs no clause
    /// for it (the argv/env subrange discipline is for borrowed data only).
    Seed([u64; 4]),
}

/// One named-grant-table entry: a well-known `name` id and what it resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub name: u8,
    pub kind: GrantKind,
}

/// A decoded (or to-be-encoded) startup block. The grant table and the
/// argv/env vectors live in fixed-size arenas with explicit counts; only the
/// first `n*` entries of each are meaningful. argv/env entries borrow into the
/// backing byte buffer (the message, for `decode`).
///
/// `Clone` is hand-written below rather than derived: Verus does not spec a
/// derived non-`Copy` `Clone` inside `verus!{}` (it would warn), and the type
/// is deliberately `Clone`-not-`Copy` to keep the few-hundred-byte struct from
/// copying silently.
#[derive(Debug)]
pub struct Startup<'a> {
    pub grants: [Grant; MAX_GRANTS],
    pub ngrants: usize,
    pub argv: [&'a [u8]; MAX_ARGV],
    pub nargv: usize,
    pub env: [&'a [u8]; MAX_ENV],
    pub nenv: usize,
}

/// `sub` is some contiguous subrange of `buf` (⊆ `buf`): the provenance fact
/// for a borrowed argv/env byte-string, the startup analog of `elf::seg_ok`'s
/// `offset + filesz <= len` file-extent bound.
pub open spec fn subseq_of(sub: Seq<u8>, buf: Seq<u8>) -> bool {
    exists|a: int, b: int| 0 <= a <= b <= buf.len() && sub == buf.subrange(a, b)
}

/// What `decode` guarantees of a returned block — the `elf::well_formed_image`
/// twin: the three counts are within their arenas, and every borrowed argv/env
/// byte-string is a subrange of the input buffer (nothing the decoder hands
/// back can index out of `buf`).
pub open spec fn well_formed_startup(s: Startup, buf: Seq<u8>) -> bool {
    &&& s.ngrants <= MAX_GRANTS
    &&& s.nargv <= MAX_ARGV
    &&& s.nenv <= MAX_ENV
    &&& forall|j: int| 0 <= j < s.nargv ==> subseq_of(#[trigger] s.argv@[j]@, buf)
    &&& forall|j: int| 0 <= j < s.nenv ==> subseq_of(#[trigger] s.env@[j]@, buf)
}

// ── Bounds-checked cursor helpers (the verified replacement for the hand-rolled
//    `Reader`): each is total — any read past the end / offset overflow yields
//    `None`, never a panic or out-of-bounds access — and advances the cursor by
//    exactly the field width, keeping it within the buffer. The fixed-width
//    little-endian fields read through the shared `le-bytes` crate's verified
//    `read_u*_le` readers (whose `requires off+N <= len` is discharged here). ──
fn take_u8(buf: &[u8], pos: usize) -> (r: Option<(u8, usize)>)
    ensures
        r matches Some((_, end)) ==> pos + 1 == end && end <= buf@.len(),
{
    broadcast use vstd::slice::group_slice_axioms;

    if pos < buf.len() {
        Some((buf[pos], pos + 1))
    } else {
        None
    }
}

fn take_u16(buf: &[u8], pos: usize) -> (r: Option<(u16, usize)>)
    ensures
        r matches Some((_, end)) ==> pos + 2 == end && end <= buf@.len(),
{
    let end = match pos.checked_add(2) {
        Some(e) => e,
        None => return None,
    };
    if end > buf.len() {
        return None;
    }
    Some((le_bytes::read_u16_le(buf, pos), end))
}

fn take_u32(buf: &[u8], pos: usize) -> (r: Option<(u32, usize)>)
    ensures
        r matches Some((_, end)) ==> pos + 4 == end && end <= buf@.len(),
{
    let end = match pos.checked_add(4) {
        Some(e) => e,
        None => return None,
    };
    if end > buf.len() {
        return None;
    }
    Some((le_bytes::read_u32_le(buf, pos), end))
}

fn take_u64(buf: &[u8], pos: usize) -> (r: Option<(u64, usize)>)
    ensures
        r matches Some((_, end)) ==> pos + 8 == end && end <= buf@.len(),
{
    let end = match pos.checked_add(8) {
        Some(e) => e,
        None => return None,
    };
    if end > buf.len() {
        return None;
    }
    Some((le_bytes::read_u64_le(buf, pos), end))
}

/// Borrow `n` bytes at `pos`, returning the slice and the advanced cursor. On
/// `Some`, the returned slice is exactly `buf[pos..pos+n]` (so ⊆ `buf`).
fn take_bytes<'a>(buf: &'a [u8], pos: usize, n: usize) -> (r: Option<(&'a [u8], usize)>)
    ensures
        r matches Some((sl, end)) ==> {
            &&& pos + n == end
            &&& end <= buf@.len()
            &&& sl@ == buf@.subrange(pos as int, end as int)
        },
{
    let end = match pos.checked_add(n) {
        Some(e) => e,
        None => return None,
    };
    if end > buf.len() {
        return None;
    }
    Some((vstd::slice::slice_subrange(buf, pos, end), end))
}

/// Decode a startup block. Mechanized **total** over arbitrary bytes (rev2§2.7):
/// for every `&[u8]`, `decode` returns without panicking or reading out of
/// bounds, and every accepted block satisfies [`well_formed_startup`] — counts
/// within their arenas and every borrowed argv/env byte-string a subrange of
/// `buf`. It validates the magic, then each count against its arena cap, then
/// bounds-checks every grant body / argv / env length against the remaining
/// slice before reading. Any shortfall, unknown grant `kind`, bad magic, or
/// over-cap count returns `None`. Trailing bytes after the last field are
/// tolerated (the `elf`/`parse_config` precedent). Returned argv/env slices
/// borrow into `buf`.
pub fn decode(buf: &[u8]) -> (res: Option<Startup<'_>>)
    ensures
        res matches Some(s) ==> well_formed_startup(s, buf@),
{
    broadcast use vstd::slice::group_slice_axioms;
    // The 7-byte header (magic + three counts) must be present.

    if buf.len() < 7 {
        return None;
    }
    // Magic `b"EUS1"`, checked byte-wise (Verus does not spec slice equality) —
    // the `elf::parse` inline-magic discipline; `MAGIC` stays the encode-side
    // const, coupled to these bytes by the round-trip oracle tests.

    if buf[0] != 0x45 || buf[1] != 0x55 || buf[2] != 0x53 || buf[3] != 0x31 {
        return None;
    }
    let ngrants = buf[4] as usize;
    let nargv = buf[5] as usize;
    let nenv = buf[6] as usize;
    if ngrants > MAX_GRANTS || nargv > MAX_ARGV || nenv > MAX_ENV {
        return None;
    }
    // Fixed-size arenas; only the first `n*` entries are meaningful. The argv/env
    // fillers are empty subranges of `buf` (borrow `buf`'s lifetime; their view
    // is irrelevant — `well_formed_startup` only constrains `j < n*`).

    let empty = vstd::slice::slice_subrange(buf, 0, 0);
    let mut grants = [Grant { name: 0, kind: GrantKind::CapSlot(0) };MAX_GRANTS];
    let mut argv: [&[u8]; MAX_ARGV] = [empty;MAX_ARGV];
    let mut env: [&[u8]; MAX_ENV] = [empty;MAX_ENV];
    let mut pos: usize = 7;

    // Grants: each entry is `name:u8, kind:u8`, then a kind-tagged body.
    let mut i: usize = 0;
    while i < ngrants
        invariant
            i <= ngrants,
            ngrants <= MAX_GRANTS,
            pos <= buf@.len(),
        decreases ngrants - i,
    {
        let (name, p0) = match take_u8(buf, pos) {
            Some(x) => x,
            None => return None,
        };
        let (tag, p1) = match take_u8(buf, p0) {
            Some(x) => x,
            None => return None,
        };
        let (kind, p_next) = if tag == KIND_CAP_SLOT {
            let (slot, q) = match take_u32(buf, p1) {
                Some(x) => x,
                None => return None,
            };
            (GrantKind::CapSlot(slot), q)
        } else if tag == KIND_STORAGE_HANDLE {
            let (handle, q) = match take_u32(buf, p1) {
                Some(x) => x,
                None => return None,
            };
            (GrantKind::StorageHandle(handle), q)
        } else if tag == KIND_REGION {
            let (va, q1) = match take_u64(buf, p1) {
                Some(x) => x,
                None => return None,
            };
            let (len, q2) = match take_u64(buf, q1) {
                Some(x) => x,
                None => return None,
            };
            let (pa, q3) = match take_u64(buf, q2) {
                Some(x) => x,
                None => return None,
            };
            (GrantKind::Region { va, len, pa }, q3)
        } else if tag == KIND_SEED {
            // Four LE words, read through the same verified `take_u64` as the
            // region body. The seed is copied by value into the `Seed` variant,
            // so — unlike argv/env — nothing is borrowed out of `buf` and there
            // is no `subseq_of` obligation to discharge; only `pos` stays bounded
            // (each `take_u64` ensures its returned cursor `<= buf.len()`).
            let (w0, q1) = match take_u64(buf, p1) {
                Some(x) => x,
                None => return None,
            };
            let (w1, q2) = match take_u64(buf, q1) {
                Some(x) => x,
                None => return None,
            };
            let (w2, q3) = match take_u64(buf, q2) {
                Some(x) => x,
                None => return None,
            };
            let (w3, q4) = match take_u64(buf, q3) {
                Some(x) => x,
                None => return None,
            };
            (GrantKind::Seed([w0, w1, w2, w3]), q4)
        } else {
            return None;
        };
        grants[i] = Grant { name, kind };
        pos = p_next;
        i = i + 1;
    }

    // Argv: each is `len:u16`, then `len` borrowed bytes (⊆ `buf`).
    let mut k: usize = 0;
    while k < nargv
        invariant
            k <= nargv,
            nargv <= MAX_ARGV,
            pos <= buf@.len(),
            forall|j: int| 0 <= j < k ==> subseq_of(#[trigger] argv@[j]@, buf@),
        decreases nargv - k,
    {
        let (len, p1) = match take_u16(buf, pos) {
            Some(x) => x,
            None => return None,
        };
        let (sl, p2) = match take_bytes(buf, p1, len as usize) {
            Some(x) => x,
            None => return None,
        };
        assert(subseq_of(sl@, buf@)) by {
            assert(0 <= p1 <= p2 <= buf@.len());
            assert(sl@ == buf@.subrange(p1 as int, p2 as int));
        }
        let ghost prev = argv@;
        let ghost prev_k = k;
        argv[k] = sl;
        proof {
            assert forall|j: int| 0 <= j < prev_k + 1 implies subseq_of(
                #[trigger] argv@[j]@,
                buf@,
            ) by {
                if j < prev_k {
                    assert(argv@[j] == prev[j]);
                } else {
                    assert(argv@[j] == sl);
                }
            }
        }
        pos = p2;
        k = k + 1;
    }

    // Env: identical shape; the finished argv quantifier rides along unchanged
    // (the loop never touches `argv`), so both survive to the return.
    let mut m: usize = 0;
    while m < nenv
        invariant
            m <= nenv,
            nargv <= MAX_ARGV,
            nenv <= MAX_ENV,
            pos <= buf@.len(),
            forall|j: int| 0 <= j < nargv ==> subseq_of(#[trigger] argv@[j]@, buf@),
            forall|j: int| 0 <= j < m ==> subseq_of(#[trigger] env@[j]@, buf@),
        decreases nenv - m,
    {
        let (len, p1) = match take_u16(buf, pos) {
            Some(x) => x,
            None => return None,
        };
        let (sl, p2) = match take_bytes(buf, p1, len as usize) {
            Some(x) => x,
            None => return None,
        };
        assert(subseq_of(sl@, buf@)) by {
            assert(0 <= p1 <= p2 <= buf@.len());
            assert(sl@ == buf@.subrange(p1 as int, p2 as int));
        }
        let ghost prev = env@;
        let ghost prev_m = m;
        env[m] = sl;
        proof {
            assert forall|j: int| 0 <= j < prev_m + 1 implies subseq_of(
                #[trigger] env@[j]@,
                buf@,
            ) by {
                if j < prev_m {
                    assert(env@[j] == prev[j]);
                } else {
                    assert(env@[j] == sl);
                }
            }
        }
        pos = p2;
        m = m + 1;
    }

    Some(Startup { grants, ngrants, argv, nargv, env, nenv })
}

} // verus!
/// Equality compares only the meaningful prefixes (`[..n*]`) of each arena, so
/// two blocks built different ways — e.g. a producer-built one and the result
/// of decoding its encoding — compare equal regardless of arena filler.
impl PartialEq for Startup<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.ngrants == other.ngrants
            && self.nargv == other.nargv
            && self.nenv == other.nenv
            && self.grants[..self.ngrants] == other.grants[..other.ngrants]
            && self.argv[..self.nargv] == other.argv[..other.nargv]
            && self.env[..self.nenv] == other.env[..other.nenv]
    }
}

impl Eq for Startup<'_> {}

/// Every field is `Copy` (`[Grant; _]`, `[&[u8]; _]`, `usize`), so the clone is
/// a field-wise copy — but the type stays `Clone`-not-`Copy` so a copy is always
/// an explicit `.clone()`. Hand-written (not derived) to keep it out of the
/// `verus!{}` block; see [`Startup`].
impl Clone for Startup<'_> {
    fn clone(&self) -> Self {
        Startup {
            grants: self.grants,
            ngrants: self.ngrants,
            argv: self.argv,
            nargv: self.nargv,
            env: self.env,
            nenv: self.nenv,
        }
    }
}

impl Default for Startup<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Why `encode` (or a builder) refused a block. The producer maps either to a
/// clean boot/spawn failure — never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// More entries than the fixed arena holds (`MAX_GRANTS`/`MAX_ARGV`/
    /// `MAX_ENV`), or a single byte-string longer than `u16::MAX`.
    TooManyEntries,
    /// The serialized block would exceed the output buffer or `MAX_BLOCK`.
    Overflow,
}

impl<'a> Startup<'a> {
    /// An empty block (no grants, no argv, no env), ready to push onto.
    pub const fn new() -> Self {
        Startup {
            grants: [Grant {
                name: NAME_STRING,
                kind: GrantKind::CapSlot(0),
            }; MAX_GRANTS],
            ngrants: 0,
            argv: [&[]; MAX_ARGV],
            nargv: 0,
            env: [&[]; MAX_ENV],
            nenv: 0,
        }
    }

    /// Append a grant. `Err(TooManyEntries)` past `MAX_GRANTS`.
    pub fn push_grant(&mut self, g: Grant) -> Result<(), EncodeError> {
        if self.ngrants >= MAX_GRANTS {
            return Err(EncodeError::TooManyEntries);
        }
        self.grants[self.ngrants] = g;
        self.ngrants += 1;
        Ok(())
    }

    /// Append an argv byte-string (borrowed). `Err(TooManyEntries)` past
    /// `MAX_ARGV`.
    pub fn push_argv(&mut self, s: &'a [u8]) -> Result<(), EncodeError> {
        if self.nargv >= MAX_ARGV {
            return Err(EncodeError::TooManyEntries);
        }
        self.argv[self.nargv] = s;
        self.nargv += 1;
        Ok(())
    }

    /// Append an env byte-string (borrowed). `Err(TooManyEntries)` past
    /// `MAX_ENV`.
    pub fn push_env(&mut self, s: &'a [u8]) -> Result<(), EncodeError> {
        if self.nenv >= MAX_ENV {
            return Err(EncodeError::TooManyEntries);
        }
        self.env[self.nenv] = s;
        self.nenv += 1;
        Ok(())
    }

    /// The resolved value of the first grant named `name`, if present.
    pub fn grant(&self, name: u8) -> Option<GrantKind> {
        self.grants[..self.ngrants]
            .iter()
            .find(|g| g.name == name)
            .map(|g| g.kind)
    }
}

// ---------------------------------------------------------------------------
// Grant projections — the pure structural readers over a decoded block. Kept here
// (with `GrantKind`) so the `eunomia_sys::grant` named-role layer and the no_std user
// drivers (`user/console`, `user/storaged`) share one definition instead of each
// re-matching `GrantKind`. No decode logic: the untrusted byte boundary is `decode`.
// ---------------------------------------------------------------------------

/// The cspace slot of the cap grant named `name`, if present and a
/// [`GrantKind::CapSlot`].
pub fn cap_slot(s: &Startup, name: u8) -> Option<u32> {
    match s.grant(name)? {
        GrantKind::CapSlot(slot) => Some(slot),
        _ => None,
    }
}

/// The handle number of the storage grant named `name`, if present and a
/// [`GrantKind::StorageHandle`].
pub fn storage_handle(s: &Startup, name: u8) -> Option<u32> {
    match s.grant(name)? {
        GrantKind::StorageHandle(h) => Some(h),
        _ => None,
    }
}

/// The `(va, len, pa)` of the region grant named `name`, if present and a
/// [`GrantKind::Region`].
pub fn region(s: &Startup, name: u8) -> Option<(u64, u64, u64)> {
    match s.grant(name)? {
        GrantKind::Region { va, len, pa } => Some((va, len, pa)),
        _ => None,
    }
}

/// The virtual address of the region grant named `name` (the field a consumer of a
/// pre-mapped page actually needs).
pub fn region_va(s: &Startup, name: u8) -> Option<u64> {
    region(s, name).map(|(va, _, _)| va)
}

/// A bounds-checked writer over the output buffer. Every write is `get_mut`-
/// checked, so `encode` is total: a write past the end yields
/// `Err(Overflow)`, never a panic or a silent truncation.
struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        let end = self
            .pos
            .checked_add(bytes.len())
            .ok_or(EncodeError::Overflow)?;
        let dst = self
            .buf
            .get_mut(self.pos..end)
            .ok_or(EncodeError::Overflow)?;
        dst.copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    fn u8(&mut self, v: u8) -> Result<(), EncodeError> {
        self.put(&[v])
    }

    fn u16(&mut self, v: u16) -> Result<(), EncodeError> {
        self.put(&v.to_le_bytes())
    }

    fn u32(&mut self, v: u32) -> Result<(), EncodeError> {
        self.put(&v.to_le_bytes())
    }

    fn u64(&mut self, v: u64) -> Result<(), EncodeError> {
        self.put(&v.to_le_bytes())
    }
}

/// Serialize `s` into `out`, returning the number of bytes written. Total: an
/// over-cap count (`TooManyEntries`) or a block that would exceed `out` or
/// `MAX_BLOCK` (`Overflow`) returns a clean `Err`, never a panic or a silent
/// truncation — the producer maps it to a boot/spawn failure. `out` is normally
/// the producer's `[u8; MAX_BLOCK]`.
pub fn encode(s: &Startup, out: &mut [u8]) -> Result<usize, EncodeError> {
    if s.ngrants > MAX_GRANTS || s.nargv > MAX_ARGV || s.nenv > MAX_ENV {
        return Err(EncodeError::TooManyEntries);
    }
    // Counts are bounded by the arena caps (<= 8), so the `as u8` casts below
    // cannot truncate.
    let mut w = Writer { buf: out, pos: 0 };
    w.put(&MAGIC)?;
    w.u8(s.ngrants as u8)?;
    w.u8(s.nargv as u8)?;
    w.u8(s.nenv as u8)?;
    for g in &s.grants[..s.ngrants] {
        w.u8(g.name)?;
        match g.kind {
            GrantKind::CapSlot(slot) => {
                w.u8(KIND_CAP_SLOT)?;
                w.u32(slot)?;
            }
            GrantKind::StorageHandle(handle) => {
                w.u8(KIND_STORAGE_HANDLE)?;
                w.u32(handle)?;
            }
            GrantKind::Region { va, len, pa } => {
                w.u8(KIND_REGION)?;
                w.u64(va)?;
                w.u64(len)?;
                w.u64(pa)?;
            }
            GrantKind::Seed(seed) => {
                w.u8(KIND_SEED)?;
                w.u64(seed[0])?;
                w.u64(seed[1])?;
                w.u64(seed[2])?;
                w.u64(seed[3])?;
            }
        }
    }
    for v in s.argv[..s.nargv].iter().chain(&s.env[..s.nenv]) {
        let len = u16::try_from(v.len()).map_err(|_| EncodeError::TooManyEntries)?;
        w.u16(len)?;
        w.put(v)?;
    }
    // Enforce the message budget even when `out` is larger than one message.
    if w.pos > MAX_BLOCK {
        return Err(EncodeError::Overflow);
    }
    Ok(w.pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A representative block exercising all three grant kinds plus a
    /// multi-element argv and an env entry.
    fn sample<'a>() -> Startup<'a> {
        let mut s = Startup::new();
        s.push_grant(Grant {
            name: NAME_TIME,
            kind: GrantKind::Region {
                va: 0xA300_0000,
                len: 4096,
                pa: 0,
            },
        })
        .unwrap();
        s.push_grant(Grant {
            name: NAME_STORAGE,
            kind: GrantKind::CapSlot(1),
        })
        .unwrap();
        s.push_grant(Grant {
            name: NAME_ROOT,
            kind: GrantKind::StorageHandle(0),
        })
        .unwrap();
        s.push_argv(b"selftest").unwrap();
        s.push_argv(b"254").unwrap();
        s.push_env(b"K=V").unwrap();
        s
    }

    #[test]
    fn golden_layout() {
        // encode → the exact bytes at the pinned offsets.
        let s = sample();
        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        let got = &buf[..n];

        // Header.
        assert_eq!(&got[0..4], b"EUS1");
        assert_eq!(got[4], 3); // ngrants
        assert_eq!(got[5], 2); // nargv
        assert_eq!(got[6], 1); // nenv

        // Grant 0: TIME region (name, kind, va, len, pa) = 26 bytes at offset 7.
        assert_eq!(got[7], NAME_TIME);
        assert_eq!(got[8], KIND_REGION);
        assert_eq!(&got[9..17], &0xA300_0000u64.to_le_bytes());
        assert_eq!(&got[17..25], &4096u64.to_le_bytes());
        assert_eq!(&got[25..33], &0u64.to_le_bytes());

        // Grant 1: STORAGE cap-slot = 6 bytes at offset 33.
        assert_eq!(got[33], NAME_STORAGE);
        assert_eq!(got[34], KIND_CAP_SLOT);
        assert_eq!(&got[35..39], &1u32.to_le_bytes());

        // Grant 2: ROOT storage-handle = 6 bytes at offset 39.
        assert_eq!(got[39], NAME_ROOT);
        assert_eq!(got[40], KIND_STORAGE_HANDLE);
        assert_eq!(&got[41..45], &0u32.to_le_bytes());

        // Argv[0] = "selftest" (len:u16 + bytes) at offset 45.
        assert_eq!(&got[45..47], &8u16.to_le_bytes());
        assert_eq!(&got[47..55], b"selftest");
        // Argv[1] = "254" at offset 55.
        assert_eq!(&got[55..57], &3u16.to_le_bytes());
        assert_eq!(&got[57..60], b"254");
        // Env[0] = "K=V" at offset 60.
        assert_eq!(&got[60..62], &3u16.to_le_bytes());
        assert_eq!(&got[62..65], b"K=V");
        assert_eq!(n, 65);

        // …and decode of those exact bytes yields the entries back.
        let d = decode(got).unwrap();
        assert_eq!(d.ngrants, 3);
        assert_eq!(d.grant(NAME_TIME), Some(s.grants[0].kind));
        assert_eq!(d.grant(NAME_STORAGE), Some(GrantKind::CapSlot(1)));
        assert_eq!(d.grant(NAME_ROOT), Some(GrantKind::StorageHandle(0)));
        assert_eq!(&d.argv[..d.nargv], &[b"selftest".as_slice(), b"254"]);
        assert_eq!(&d.env[..d.nenv], &[b"K=V".as_slice()]);
    }

    #[test]
    fn grant_projections() {
        // The pure GrantKind projections read each kind out of a decoded block and
        // reject a wrong-kind lookup (None, not a misread) — the readers the
        // `eunomia_sys::grant` layer and the no_std user drivers share.
        let s = sample();
        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        let d = decode(&buf[..n]).unwrap();

        assert_eq!(region(&d, NAME_TIME), Some((0xA300_0000, 4096, 0)));
        assert_eq!(region_va(&d, NAME_TIME), Some(0xA300_0000));
        assert_eq!(cap_slot(&d, NAME_STORAGE), Some(1));
        assert_eq!(storage_handle(&d, NAME_ROOT), Some(0));

        // Absent grant → None.
        assert_eq!(region(&d, NAME_DMA), None);
        // Wrong-kind lookup → None (a cap-slot asked of a region grant, and vice
        // versa), never a misread.
        assert_eq!(cap_slot(&d, NAME_TIME), None);
        assert_eq!(region(&d, NAME_STORAGE), None);
        assert_eq!(storage_handle(&d, NAME_STORAGE), None);
        assert_eq!(region_va(&d, NAME_STORAGE), None);
    }

    #[test]
    fn seed_grant_golden() {
        // A block carrying one NAME_RANDOM_SEED grant: name, kind, then the four
        // LE seed words at the pinned offsets, and decode reads them back.
        let seed = [0x0011_2233_4455_6677u64, 0x8899_AABB_CCDD_EEFF, 1, u64::MAX];
        let mut s = Startup::new();
        s.push_grant(Grant {
            name: NAME_RANDOM_SEED,
            kind: GrantKind::Seed(seed),
        })
        .unwrap();
        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        let got = &buf[..n];

        // Header: one grant, no argv/env.
        assert_eq!(&got[0..4], b"EUS1");
        assert_eq!(got[4], 1);
        assert_eq!(got[5], 0);
        assert_eq!(got[6], 0);
        // Grant 0: SEED = name, kind, 4×u64 = 34 bytes at offset 7.
        assert_eq!(got[7], NAME_RANDOM_SEED);
        assert_eq!(got[8], KIND_SEED);
        assert_eq!(&got[9..17], &seed[0].to_le_bytes());
        assert_eq!(&got[17..25], &seed[1].to_le_bytes());
        assert_eq!(&got[25..33], &seed[2].to_le_bytes());
        assert_eq!(&got[33..41], &seed[3].to_le_bytes());
        assert_eq!(n, 41);

        // …and decode of those exact bytes yields the seed back.
        let d = decode(got).unwrap();
        assert_eq!(d.ngrants, 1);
        assert_eq!(d.grant(NAME_RANDOM_SEED), Some(GrantKind::Seed(seed)));
    }

    #[test]
    fn decode_refuses_truncated_seed() {
        // A SEED grant declared but its 32-byte body cut short → None (the
        // region-truncation discipline extended to the new kind).
        let mut t = Vec::new();
        t.extend_from_slice(&MAGIC);
        t.extend_from_slice(&[1, 0, 0]); // one grant, no argv/env
        t.push(NAME_RANDOM_SEED);
        t.push(KIND_SEED);
        t.extend_from_slice(&[0u8; 20]); // only 20 of the 32 seed bytes
        assert_eq!(decode(&t), None);
    }

    #[test]
    fn decode_tolerates_trailing_bytes() {
        let s = sample();
        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        // The recv buffer is always [u8; 256]; a decode of the whole padded
        // buffer (trailing zeros past the block) must still succeed.
        assert_eq!(decode(&buf), Some(s));
    }

    #[test]
    fn rejects_malformed() {
        // Empty and a wrong magic.
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(b"EUS0\x00\x00\x00"), None);
        // Header truncated (magic but no counts).
        assert_eq!(decode(b"EUS1"), None);

        // A well-formed prefix whose single grant body is truncated.
        // ngrants=1, nargv=0, nenv=0, then a REGION grant with only 3 of its
        // 24 body bytes present.
        let mut t = Vec::new();
        t.extend_from_slice(&MAGIC);
        t.extend_from_slice(&[1, 0, 0]); // ngrants, nargv, nenv
        t.push(NAME_TIME);
        t.push(KIND_REGION);
        t.extend_from_slice(&[0u8; 3]); // far short of 24
        assert_eq!(decode(&t), None);

        // An unknown grant kind byte.
        let mut k = Vec::new();
        k.extend_from_slice(&MAGIC);
        k.extend_from_slice(&[1, 0, 0]);
        k.push(NAME_TIME);
        k.push(99); // not a KIND_*
        k.extend_from_slice(&[0u8; 24]);
        assert_eq!(decode(&k), None);

        // A count over the arena cap is refused before any body read.
        let mut c = Vec::new();
        c.extend_from_slice(&MAGIC);
        c.extend_from_slice(&[(MAX_GRANTS as u8) + 1, 0, 0]);
        assert_eq!(decode(&c), None);

        // An argv length that runs past the buffer.
        let mut a = Vec::new();
        a.extend_from_slice(&MAGIC);
        a.extend_from_slice(&[0, 1, 0]); // one argv
        a.extend_from_slice(&255u16.to_le_bytes()); // declares 255 bytes…
        a.extend_from_slice(b"short"); // …but only 5 follow
        assert_eq!(decode(&a), None);
    }

    #[test]
    fn encode_refuses_over_budget() {
        // An argv vector whose bytes blow past the 256-byte budget: encode must
        // return Err(Overflow), never panic or truncate. Each entry is 200
        // bytes; two of them exceed MAX_BLOCK.
        let big = [b'x'; 200];
        let mut s = Startup::new();
        s.push_argv(&big).unwrap();
        s.push_argv(&big).unwrap();
        let mut buf = [0u8; MAX_BLOCK];
        assert_eq!(encode(&s, &mut buf), Err(EncodeError::Overflow));

        // A too-small output buffer also refuses cleanly.
        let mut tiny = [0u8; 4];
        assert_eq!(encode(&sample(), &mut tiny), Err(EncodeError::Overflow));
    }

    #[test]
    fn builders_refuse_past_the_arena() {
        let mut s = Startup::new();
        for _ in 0..MAX_GRANTS {
            s.push_grant(Grant {
                name: NAME_TIME,
                kind: GrantKind::CapSlot(0),
            })
            .unwrap();
        }
        assert_eq!(
            s.push_grant(Grant {
                name: NAME_TIME,
                kind: GrantKind::CapSlot(0),
            }),
            Err(EncodeError::TooManyEntries)
        );
    }

    /// Negative control (anti-theater): the round-trip equality oracle must
    /// have teeth — perturbing one field of the decoded block makes it compare
    /// *un*equal, so the round-trip proptest below is not vacuously true.
    #[test]
    fn round_trip_oracle_has_teeth() {
        let s = sample();
        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        let got = decode(&buf[..n]).unwrap();
        assert_eq!(got, s); // the real round-trip holds…

        // …and a single perturbed grant name is distinguished.
        let mut wrong = s.clone();
        wrong.grants[0].name ^= 1;
        assert_ne!(got, wrong);
        // As is a perturbed argv element and a changed count.
        let mut wrong_argv = s.clone();
        wrong_argv.argv[0] = b"SELFTEST";
        assert_ne!(got, wrong_argv);
        let mut wrong_n = s.clone();
        wrong_n.nargv = 1;
        assert_ne!(got, wrong_n);
    }

    // Bounded strategies so an encoded block stays within the 256-byte budget:
    // grants/argv/env counts within their arenas, short byte-strings, and a
    // grant kind drawn from all three.
    fn grant_kind() -> impl Strategy<Value = GrantKind> {
        prop_oneof![
            any::<u32>().prop_map(GrantKind::CapSlot),
            any::<u32>().prop_map(GrantKind::StorageHandle),
            (any::<u64>(), any::<u64>(), any::<u64>())
                .prop_map(|(va, len, pa)| GrantKind::Region { va, len, pa }),
            any::<[u64; 4]>().prop_map(GrantKind::Seed),
        ]
    }

    fn grant() -> impl Strategy<Value = Grant> {
        (any::<u8>(), grant_kind()).prop_map(|(name, kind)| Grant { name, kind })
    }

    fn byte_strings(max: usize) -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop::collection::vec(prop::collection::vec(any::<u8>(), 0..12), 0..=max)
    }

    proptest! {
        // Miri runs the interpreter; a handful of cases cover the same logic,
        // native keeps the full sweep (the cas/file.rs, layout_props idiom).
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// encode → decode round-trips every block within the arena caps.
        #[test]
        fn round_trips(
            grants in prop::collection::vec(grant(), 0..=MAX_GRANTS),
            argv in byte_strings(MAX_ARGV),
            env in byte_strings(MAX_ENV),
        ) {
            let mut s = Startup::new();
            for g in &grants {
                s.push_grant(*g).unwrap();
            }
            for a in &argv {
                s.push_argv(a).unwrap();
            }
            for e in &env {
                s.push_env(e).unwrap();
            }
            let mut buf = [0u8; MAX_BLOCK];
            // Within-arena counts and short byte-strings, but a full table of
            // region grants can still exceed 256 bytes. encode is total: it
            // either fits (and round-trips) or refuses cleanly — and since the
            // counts are within the arena and no string exceeds u16, the only
            // legal refusal is the budget (`Overflow`), never `TooManyEntries`.
            match encode(&s, &mut buf) {
                Ok(n) => prop_assert_eq!(decode(&buf[..n]), Some(s)),
                Err(e) => prop_assert_eq!(e, EncodeError::Overflow),
            }
        }

        /// Total over arbitrary bytes: `decode` never panics — the rev2§2.7
        /// refuse-not-crash floor (the shape of `parse_config_is_total`).
        #[test]
        fn decode_is_total(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            if let Some(s) = decode(&bytes) {
                // Whatever it returns obeys the arena bounds and every borrowed
                // slice lies inside the input.
                prop_assert!(s.ngrants <= MAX_GRANTS);
                prop_assert!(s.nargv <= MAX_ARGV);
                prop_assert!(s.nenv <= MAX_ENV);
                let range = bytes.as_ptr_range();
                for v in s.argv[..s.nargv].iter().chain(&s.env[..s.nenv]) {
                    if !v.is_empty() {
                        let r = v.as_ptr_range();
                        prop_assert!(r.start >= range.start && r.end <= range.end);
                    }
                }
            }
        }
    }
}
