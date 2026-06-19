//! Storage server — userspace process holding the virtio-blk cap (spec rev0§4).
//!
//! The session/handle/dispatch core lives in lib.rs and is host-testable.
//! This binary is the on-OS server (real processes + IPC transport over
//! channel sessions); the host build has nothing to run.

fn main() {
    eprintln!(
        "storage-server: session core is library-only until the M3 IPC \
         transport exists (see storage_server::Server)"
    );
}
