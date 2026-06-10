#![no_std]
#![no_main]

mod aspace;
mod boot;
mod channel;
mod cspace;
mod exceptions;
mod gic;
mod mmu;
mod notification;
mod syscall;
mod thread;
mod timer;
mod uart;
mod untyped;
mod user;

use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};
use cspace::{Cap, CapKind, CapSlot, CSpaceObj, ObjHeader, Rights};
use thread::{Tcb, ThreadState, TrapFrame};

/// Init's cspace, statically allocated: the one kernel object not carved
/// from untyped — morally init's memory baked into the image (§3.2 note
/// in untyped.rs).
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
    writeln!(out, "\nEunomia OS — M1").unwrap();

    mmu::init();
    gic::init();
    timer::start_tick();
    writeln!(out, "MMU + GICv3 + tick @ {} Hz up", timer::TICK_HZ).unwrap();

    unsafe {
        let root = addr_of_mut!(ROOT_CSPACE.obj);

        // Slot 0: all free DRAM between the kernel image and the user
        // window, as init's untyped. Every kernel object the test creates
        // is carved from this (§2.5, §3.2).
        let ut_base = (addr_of!(__kernel_end) as u64 + 0xFFF) & !0xFFF;
        let slot0 = CSpaceObj::slot(root, 0);
        (*slot0).cap = Cap {
            kind: CapKind::Untyped {
                base: ut_base,
                size: mmu::USER_BASE - ut_base,
                watermark: 0,
            },
            rights: Rights::ALL,
        };
        writeln!(
            out,
            "init untyped: {:#x}..{:#x} ({} KiB)",
            ut_base,
            mmu::USER_BASE,
            (mmu::USER_BASE - ut_base) / 1024
        )
        .unwrap();

        // Init thread: the embedded EL0 test program (user.rs), with a
        // cap to itself in slot 1.
        let init = addr_of_mut!(INIT_TCB);
        (*init).cspace = root;
        (*root).hdr.refs += 1;
        (*init).priority = 16;
        (*init).frame = TrapFrame::zeroed();
        (*init).frame.elr = (user::user_main as extern "C" fn(u64) -> !) as usize as u64;
        (*init).frame.sp_el0 = user::USER_STACK_TOP;
        (*init).frame.spsr = 0; // EL0t, interrupts unmasked
        (*init).state = ThreadState::Running;
        let slot1 = CSpaceObj::slot(root, 1);
        (*slot1).cap = Cap {
            kind: CapKind::Thread(init),
            rights: Rights::ALL,
        };

        // Idle: EL0 WFI loop, priority 0, always ready (§5.4).
        let idle = addr_of_mut!(IDLE_TCB);
        (*idle).priority = 0;
        (*idle).frame = TrapFrame::zeroed();
        (*idle).frame.elr = (user::user_idle as extern "C" fn(u64) -> !) as usize as u64;
        (*idle).frame.sp_el0 = user::T2_STACK_TOP - 0x1_0000;
        (*idle).frame.spsr = 0;
        thread::enqueue(idle);

        thread::set_current(init);
        writeln!(out, "entering EL0").unwrap();
        enter_first_thread(&(*init).frame);
    }
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
