#![no_std]
#![no_main]

mod aspace;
mod boot;
mod channel;
mod cspace;
mod exceptions;
mod gic;
mod irq;
mod mmu;
mod notification;
mod store;
mod syscall;
mod thread;
mod timer;
mod uart;
mod untyped;
mod user;

use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};
use cspace::{CSpaceObj, Cap, CapKind, CapSlot, ObjHeader, Rights};
use kcore::id::ObjId;
use thread::{Tcb, ThreadState, TrapFrame};

/// Init's cspace, statically allocated: the one kernel object not carved
/// from untyped — morally init's memory baked into the image.
#[repr(C)]
struct RootCSpace {
    obj: CSpaceObj,
    slots: [CapSlot; 64],
}

static mut ROOT_CSPACE: RootCSpace = RootCSpace {
    obj: CSpaceObj {
        hdr: ObjHeader { refs: 1 },
        num_slots: 64,
    },
    slots: [const { CapSlot::empty() }; 64],
};

static mut INIT_TCB: Tcb = Tcb::empty();
static mut IDLE_TCB: Tcb = Tcb::empty();

extern "C" {
    static __kernel_end: u8;
}

#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    exceptions::init();

    let mut out = uart::Uart::new();
    writeln!(out, "\nEunomia OS").unwrap();

    mmu::init();
    gic::init();
    irq::init(); // route + enable the boot-static device SPIs (B-IRQ-B)
    timer::start_tick();
    writeln!(out, "MMU + GICv3 + tick @ {} Hz up", timer::TICK_HZ).unwrap();

    unsafe {
        let root = addr_of_mut!(ROOT_CSPACE.obj);
        let init = addr_of_mut!(INIT_TCB);

        let (untyped_base, init_aspace) = setup_init(root, init, &mut out);

        // Slot 0: all remaining free DRAM below the EL0 window as init's
        // untyped — the root of every grant in the system (rev1§1, rev1§2.5).
        // Boot untypeds carry phys-read (rev1§2.5): frames retyped from them
        // inherit it, and init alone decides where it propagates.
        let slot0 = CSpaceObj::slot(root, 0);
        (*slot0).cap = Cap {
            kind: CapKind::Untyped {
                base: untyped_base,
                size: mmu::USER_BASE - untyped_base,
                watermark: 0,
            },
            rights: Rights(Rights::READ | Rights::WRITE | Rights::PHYS),
        };
        // Slot 2: the DRAM above the identity user window, for delegation
        // (the shell's spawner draws from it).
        let slot2 = CSpaceObj::slot(root, 2);
        (*slot2).cap = Cap {
            kind: CapKind::Untyped {
                base: mmu::USER_BASE + mmu::USER_SIZE,
                size: 0x5000_0000 - (mmu::USER_BASE + mmu::USER_SIZE),
                watermark: 0,
            },
            rights: Rights(Rights::READ | Rights::WRITE | Rights::PHYS),
        };
        writeln!(
            out,
            "init untyped: {:#x}..{:#x} ({} KiB)",
            untyped_base,
            mmu::USER_BASE,
            (mmu::USER_BASE - untyped_base) / 1024
        )
        .unwrap();

        // Slot 3: the virtio-mmio window (32 transports × 0x200 at
        // 0x0a00_0000 on QEMU virt) as a phys-capable device frame.
        // Init delegates it (or an attenuated copy) to the one DMA
        // driver; phys-read never enters ordinary derivation (rev1§2.5).
        let slot3 = CSpaceObj::slot(root, 3);
        (*slot3).cap = Cap {
            kind: CapKind::Frame {
                base: 0x0a00_0000,
                pages: 4,
                mapping: None,
            },
            rights: Rights(Rights::READ | Rights::WRITE | Rights::PHYS),
        };

        // Slot 4: the PL031 RTC frame (QEMU virt), read-only — init reads
        // RTCDR once at boot to seed the time page (rev1§2.6) and the device
        // is never touched again; no write authority exists anywhere.
        let slot4 = CSpaceObj::slot(root, 4);
        (*slot4).cap = Cap {
            kind: CapKind::Frame {
                base: 0x0901_0000,
                pages: 1,
                mapping: None,
            },
            rights: Rights(Rights::READ | Rights::PHYS),
        };

        // The PL011 console's device resources for init (rev1§1: init "holds
        // all device resources … MMIO frames, IRQ caps") — the device-MMIO-frame
        // precedent (slots 3/4), boot-static not retyped (B-IRQ Design dec. 3).
        //
        // Gated to the m1-test path. The real-boot init's 64-slot cspace is
        // fully hand-packed — `user/init` lays out its allocations (6..=18) and
        // the storaged/shell spawn windows (`SD_SPAWN_BASE = 20`, slots 20.. used
        // by `spawn::prepare` as aspace/tcb/cspace/segment frames; `SH_SPAWN_BASE
        // = 40`), so any fixed slot here collides with a child's spawn scratch
        // (slot 23/24 are storaged's first segment frames). The real-boot init
        // also has no consumer for these caps until the console driver exists.
        // So the real-boot grant + the init-cspace restructuring it needs land
        // with C-M9 (which spawns the driver and delegates these caps); B-IRQ-C
        // proves the mechanism on the m1-test init, whose ROOT_CSPACE is free
        // above the exerciser's retype range (6..=22).
        #[cfg(feature = "m1-test")]
        {
            // Slot 23: the PL011 MMIO frame (UART at 0x0900_0000 on QEMU virt) —
            // the driver's register window (held for delegation; m1-test does
            // not map it — the EL0 exerciser has no aspace).
            let slot23 = CSpaceObj::slot(root, 23);
            (*slot23).cap = Cap {
                kind: CapKind::Frame {
                    base: 0x0900_0000,
                    pages: 1,
                    mapping: None,
                },
                rights: Rights(Rights::READ | Rights::WRITE | Rights::PHYS),
            };
            // Slot 24: the PL011 IRQ-handler cap (rev1§1) — INTID 33 (SPI 1, RX).
            // A plain designating handle to the boot-static `IrqObj`; bound via
            // `IrqBind` / cleared via `IrqAck`. The handlers gate on the cap
            // kind, not its rights, so READ|WRITE just keeps it delegable.
            let slot24 = CSpaceObj::slot(root, 24);
            (*slot24).cap = Cap {
                kind: CapKind::Irq(irq::pl011_objid()),
                rights: Rights(Rights::READ | Rights::WRITE),
            };
        }

        // Slot 5: init's own address space. Init is the one process that
        // maps things into itself (the PL031 window for the boot-time RTC
        // read, rev1§2.6); children never hold their own aspace caps.
        if !init_aspace.is_null() {
            let slot5 = CSpaceObj::slot(root, 5);
            (*slot5).cap = Cap {
                kind: CapKind::Aspace(ObjId(init_aspace as u64)),
                rights: Rights::ALL,
            };
            (*init_aspace).hdr.refs += 1;
        }

        // Slot 1: init's own thread cap (creator-grade: the rev1§2.3 thread
        // bits, like any retyped TCB's first cap).
        (*init).cspace = Some(ObjId(root as u64));
        (*root).hdr.refs += 1;
        (*init).priority = 16;
        (*init).state = ThreadState::Running;
        let slot1 = CSpaceObj::slot(root, 1);
        (*slot1).cap = Cap {
            // rev1§5.4 ceiling = init's own priority: init is the root of the
            // priority lattice; every retyped descendant cap is capped at its
            // retyper's priority (kernel/src/untyped.rs), so the lattice is
            // rooted here.
            kind: CapKind::Thread(ObjId(init as u64), (*init).priority),
            rights: Rights::THREAD_ALL,
        };

        // Idle: EL0 WFI loop in the identity window, priority 0 (rev1§5.4).
        let idle = addr_of_mut!(IDLE_TCB);
        (*idle).priority = 0;
        (*idle).frame = TrapFrame::zeroed();
        (*idle).frame.elr = (user::user_idle as extern "C" fn(u64) -> !) as usize as u64;
        (*idle).frame.sp_el0 = mmu::USER_BASE + mmu::USER_SIZE - 0x2_0000;
        (*idle).frame.spsr = 0;
        thread::enqueue(idle);

        thread::set_current(init);
        thread::activate_aspace(init);
        writeln!(out, "entering EL0").unwrap();
        enter_first_thread(&(*init).frame);
    }
}

/// M1 regression path: the embedded identity-window test program.
#[cfg(feature = "m1-test")]
unsafe fn setup_init(
    _root: *mut CSpaceObj,
    init: *mut Tcb,
    out: &mut uart::Uart,
) -> (u64, *mut aspace::AspaceObj) {
    writeln!(out, "boot: M1 embedded test").unwrap();
    (*init).frame = TrapFrame::zeroed();
    (*init).frame.elr = (user::user_main as extern "C" fn(u64) -> !) as usize as u64;
    (*init).frame.sp_el0 = user::USER_STACK_TOP;
    (*init).frame.spsr = 0;
    (
        (addr_of!(__kernel_end) as u64 + 0xFFF) & !0xFFF,
        core::ptr::null_mut(),
    )
}

/// Real boot (rev1§1): construct exactly one process — init — by loading the
/// embedded init ELF into a fresh address space. Everything not carved
/// here becomes init's untyped.
#[cfg(not(feature = "m1-test"))]
unsafe fn setup_init(
    _root: *mut CSpaceObj,
    init: *mut Tcb,
    out: &mut uart::Uart,
) -> (u64, *mut aspace::AspaceObj) {
    use aspace::{AspaceObj, PAGE, PERM_W, PERM_X};

    static INIT_ELF: &[u8] = include_bytes!(env!("INIT_ELF_PATH"));
    const STACK_TOP: u64 = 0x9000_0000;
    const STACK_PAGES: u64 = 16;

    let mut bump = (addr_of!(__kernel_end) as u64 + PAGE - 1) & !(PAGE - 1);
    let mut carve = |bytes: u64| {
        let p = bump;
        bump = (bump + bytes + PAGE - 1) & !(PAGE - 1);
        p
    };

    let asp = carve(AspaceObj::bytes_for(16) as u64) as *mut AspaceObj;
    aspace::init(asp, 16);

    let img = loader::elf::parse(INIT_ELF).expect("embedded init ELF is malformed");
    for seg in &img.segments[..img.nsegments] {
        let va_start = seg.vaddr & !(PAGE - 1);
        let va_end = (seg.vaddr + seg.memsz + PAGE - 1) & !(PAGE - 1);
        let pages = (va_end - va_start) / PAGE;
        let pa = carve(pages * PAGE);
        core::ptr::write_bytes(pa as *mut u8, 0, (pages * PAGE) as usize);
        core::ptr::copy_nonoverlapping(
            INIT_ELF.as_ptr().add(seg.offset as usize),
            (pa + (seg.vaddr - va_start)) as *mut u8,
            seg.filesz as usize,
        );
        let mut perms = 0;
        if seg.flags & loader::elf::PF_W != 0 {
            perms |= PERM_W;
        }
        if seg.flags & loader::elf::PF_X != 0 {
            perms |= PERM_X;
        }
        aspace::map(asp, pa, va_start, pages, perms).expect("mapping init segment");
    }

    let stack_pa = carve(STACK_PAGES * PAGE);
    core::ptr::write_bytes(stack_pa as *mut u8, 0, (STACK_PAGES * PAGE) as usize);
    aspace::map(
        asp,
        stack_pa,
        STACK_TOP - STACK_PAGES * PAGE,
        STACK_PAGES,
        PERM_W,
    )
    .expect("mapping init stack");

    (*asp).hdr.refs += 1; // init thread's reference
    (*init).aspace = Some(ObjId(asp as u64));
    (*init).frame = TrapFrame::zeroed();
    (*init).frame.elr = img.entry;
    (*init).frame.sp_el0 = STACK_TOP;
    (*init).frame.spsr = 0;

    writeln!(out, "boot: init ELF loaded, entry {:#x}", img.entry).unwrap();
    (bump, asp)
}

/// Drop into EL0 for the first time: load the frame's PC/SP/PSTATE, clear
/// every GPR (no kernel values may leak to EL0), eret.
unsafe fn enter_first_thread(frame: &TrapFrame) -> ! {
    core::arch::asm!(
        "msr sp_el0, {sp}",
        "msr elr_el1, {elr}",
        "msr spsr_el1, {spsr}",
        "mov x0, xzr", "mov x1, xzr", "mov x2, xzr", "mov x3, xzr",
        "mov x4, xzr", "mov x5, xzr", "mov x6, xzr", "mov x7, xzr",
        "mov x8, xzr", "mov x9, xzr", "mov x10, xzr", "mov x11, xzr",
        "mov x12, xzr", "mov x13, xzr", "mov x14, xzr", "mov x15, xzr",
        "mov x16, xzr", "mov x17, xzr", "mov x18, xzr", "mov x19, xzr",
        "mov x20, xzr", "mov x21, xzr", "mov x22, xzr", "mov x23, xzr",
        "mov x24, xzr", "mov x25, xzr", "mov x26, xzr", "mov x27, xzr",
        "mov x28, xzr", "mov x29, xzr", "mov x30, xzr",
        "eret",
        sp = in(reg) frame.sp_el0,
        elr = in(reg) frame.elr,
        spsr = in(reg) frame.spsr,
        options(noreturn),
    )
}

#[panic_handler]
fn on_panic(info: &core::panic::PanicInfo) -> ! {
    let mut out = uart::Uart::new();
    let _ = writeln!(out, "\nKERNEL PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
