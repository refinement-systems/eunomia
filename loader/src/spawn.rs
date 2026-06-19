//! Spawn-with-caps (rev0§5, rev0§5.1): build a child process from an ELF
//! image and an explicitly constructed cspace.
//!
//! The caller owns the policy: `prepare` creates the kernel objects and
//! maps the image; the caller then installs whatever caps the child
//! should start with (bootstrap channel in slot 0 — rev0§5.1 — plus any
//! grants) via `ipc::sys::cap_install`, and finally calls `start`.

use crate::elf::{self, ElfError, PF_W, PF_X};
use ipc::sys::{self, OBJ_ASPACE, OBJ_CSPACE, OBJ_FRAME, OBJ_THREAD, PERM_W, PERM_X};

pub const PAGE: u64 = 4096;
/// Fixed-size stack with an unmapped guard region below it (rev0§5.3).
pub const STACK_TOP: u64 = 0x9000_0000;
pub const STACK_PAGES: u64 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    Elf(ElfError),
    Sys(i64),
    TooManySegments,
}

fn check(r: i64) -> Result<(), SpawnError> {
    if r < 0 { Err(SpawnError::Sys(r)) } else { Ok(()) }
}

/// Slot layout the spawner consumes: `base` .. `base+3+nsegments+1` in
/// the caller's cspace (aspace, tcb, child cspace, one frame per
/// segment, stack frame). All must be free.
pub struct Prepared {
    pub aspace_slot: u32,
    pub tcb_slot: u32,
    pub cspace_slot: u32,
    pub entry: u64,
    pub sp: u64,
}

pub fn prepare(
    image: &[u8],
    untyped: u32,
    base: u32,
    child_cspace_slots: u64,
) -> Result<Prepared, SpawnError> {
    let img = elf::parse(image).map_err(SpawnError::Elf)?;

    let aspace_slot = base;
    let tcb_slot = base + 1;
    let cspace_slot = base + 2;
    // Table pool: 16 pages covers several GiB-crossing mappings at MVP scale.
    check(sys::retype(untyped, OBJ_ASPACE, 16, aspace_slot, 0))?;
    check(sys::retype(untyped, OBJ_THREAD, 0, tcb_slot, 0))?;
    check(sys::retype(untyped, OBJ_CSPACE, child_cspace_slots, cspace_slot, 0))?;

    for (i, seg) in img.segments[..img.nsegments].iter().enumerate() {
        let frame_slot = base + 3 + i as u32;
        let va_start = seg.vaddr & !(PAGE - 1);
        let va_end = (seg.vaddr + seg.memsz + PAGE - 1) & !(PAGE - 1);
        let pages = (va_end - va_start) / PAGE;
        check(sys::retype(untyped, OBJ_FRAME, pages, frame_slot, 0))?;
        // Frames are zeroed at retype, so bss needs no explicit clear.
        let file = &image[seg.offset as usize..(seg.offset + seg.filesz) as usize];
        check(sys::frame_write(frame_slot, seg.vaddr - va_start, file))?;
        let mut perms = 0;
        if seg.flags & PF_W != 0 {
            perms |= PERM_W;
        }
        if seg.flags & PF_X != 0 {
            perms |= PERM_X;
        }
        check(sys::map(aspace_slot, frame_slot, va_start, perms))?;
    }

    let stack_slot = base + 3 + img.nsegments as u32;
    check(sys::retype(untyped, OBJ_FRAME, STACK_PAGES, stack_slot, 0))?;
    check(sys::map(
        aspace_slot,
        stack_slot,
        STACK_TOP - STACK_PAGES * PAGE,
        PERM_W,
    ))?;

    Ok(Prepared {
        aspace_slot,
        tcb_slot,
        cspace_slot,
        entry: img.entry,
        sp: STACK_TOP,
    })
}

pub fn start(p: &Prepared, prio: u64) -> Result<(), SpawnError> {
    check(sys::thread_start_as(
        p.tcb_slot,
        p.cspace_slot,
        p.aspace_slot,
        p.entry,
        p.sp,
        prio,
    ))
}
