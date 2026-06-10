//! Storage server — userspace process holding the virtio-blk cap (spec §4).
//!
//! The session/handle/dispatch core lives in lib.rs and is host-testable.
//! This binary becomes the on-OS server at M3 (real processes + IPC
//! transport over channel sessions); until then it has nothing to run.

fn main() {
    eprintln!(
        "storage-server: session core is library-only until the M3 IPC \
         transport exists (see storage_server::Server)"
    );
}
