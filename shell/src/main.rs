//! Command-line shell (spec §7, M3–M4).
//!
//! Receives a storage session pre-populated with a snapshot handle and a
//! console channel in its startup block (spec §5.1). Built-ins for MVP:
//!   run <path>    — load and spawn a program from the versioned store
//!   snapshot      — take a snapshot of the current ref
//!   rollback <id> — switch the ref head to snapshot <id>
//!   ls [path]     — list directory via storage handle
//!   cat <path>    — dump file contents via storage handle
//!
//! M3 work items: startup block parsing, storage session ops, spawn
//! M4 work items: snapshot/rollback built-ins

fn main() {
    todo!("M3: shell main loop")
}
