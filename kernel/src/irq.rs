//! Kernel-side IRQ surface (rev1§1, rev1§3.6): the trusted int→ptr shell over the
//! verified [`kcore::irq`] object core — the twin of [`crate::timer`]'s tick shell.
//!
//! kcore owns the binding/census logic (`irq_bind`/`irq_unbind`/`destroy_irq`,
//! reached through the [`kcore::store::Store`] seam); this module keeps what is
//! architectural and trusted (rev1§6.1(c)/(d)): the boot-static `IRQ_TABLE` of
//! `IrqObj` (Design decision 3 — the device-MMIO-frame precedent, *not* retyped),
//! the INTID→object lookup (the `ARMED_HEAD`-resolution analog), the device-IRQ
//! delivery path (mask-on-deliver + the verified `notification::signal`), and the
//! per-IRQ GIC mask/unmask the `IrqBind`/`IrqAck` syscalls drive.

use crate::store::KernelStore;
use kcore::id::ObjId;
use kcore::irq::IrqObj;
use kcore::notification::{self, NotifObj};

/// PL011 RX is SPI 1 → INTID 33 on QEMU virt (rev1§7's console line).
pub const PL011_INTID: u32 = 33;

/// The boot-static device-SPI set. Sized for the platform's device IRQs (just the
/// PL011 console line at MVP; room to grow — adding a line is a table + boot-grant
/// addition, not new verified code).
const N_SPI: usize = 1;

/// The fixed table of IRQ objects, baked into the kernel image. kcore addresses
/// these through `Store::irq_*`; the trusted shell here derefs them directly.
static mut IRQ_TABLE: [IrqObj; N_SPI] = [IrqObj::boot_static(PL011_INTID)];

#[inline]
unsafe fn slot(i: usize) -> *mut IrqObj {
    core::ptr::addr_of_mut!(IRQ_TABLE[i])
}

/// Trusted INTID→object lookup (the `ARMED_HEAD`-resolution analog, rev1§6.1(d)).
unsafe fn irq_for_intid(intid: u32) -> Option<*mut IrqObj> {
    (0..N_SPI).map(|i| slot(i)).find(|&p| (*p).intid == intid)
}

/// Boot: route + enable each boot-static device SPI at the GIC distributor.
/// Called from `kernel_main` after `gic::init`.
pub fn init() {
    unsafe {
        for i in 0..N_SPI {
            let p = slot(i);
            crate::gic::set_route((*p).intid);
            crate::gic::enable((*p).intid);
        }
    }
}

/// Device-IRQ delivery (Design decision 2): on a *bound* INTID, mask the line and
/// signal its bound notification through the **verified** `notification::signal`
/// (the primitive the timer's `check_expired` uses). Returns whether a
/// notification was signalled, so the caller can hint a reschedule. An *unbound*
/// INTID returns `false` — the caller EOIs and drops it (no receiver).
///
/// Masking before the caller's EOI is what keeps a still-asserted level-triggered
/// line (the driver has not yet read the device) from immediately re-pending;
/// `IrqAck` unmasks once the driver has serviced it.
pub unsafe fn deliver(intid: u32) -> bool {
    if let Some(p) = irq_for_intid(intid) {
        if (*p).bound {
            // `irq_wf`: a bound IRQ always carries `notif is Some`.
            let notif = (*p).notif.unwrap();
            let bits = (*p).bits;
            crate::gic::disable(intid);
            (*p).masked = true;
            notification::signal(&mut KernelStore, notif, bits);
            return true;
        }
    }
    false
}

/// `IrqBind` core (⟵ `timer::arm` wrapper): bind the IRQ object to a
/// (notification, bits) pair via the verified [`kcore::irq::irq_bind`].
pub unsafe fn bind(i: *mut IrqObj, notif: *mut NotifObj, bits: u64) {
    kcore::irq::irq_bind(&mut KernelStore, ObjId(i as u64), ObjId(notif as u64), bits);
}

/// `IrqAck` core: clear the mask and re-enable the line so the next interrupt is
/// delivered (the "acknowledge" half of rev1§1's "receive and acknowledge").
pub unsafe fn ack(i: *mut IrqObj) {
    (*i).masked = false;
    crate::gic::enable((*i).intid);
}
