//! Seed-corpus generator for loader/fuzz. Emits minimal valid ELF64
//! aarch64 ET_EXEC images so the fuzzer mutates real header/segment fields
//! from a parseable base. Run: `cargo run -p loader --example gen_loader_corpus`.

use std::fs;
use std::path::PathBuf;

use loader::elf::{PF_R, PF_W, PF_X};

fn write_seed(name: &str, bytes: &[u8]) {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("fuzz");
    p.push("corpus");
    p.push("elf_parse");
    fs::create_dir_all(&p).unwrap();
    p.push(name);
    fs::write(&p, bytes).unwrap();
    println!("  elf_parse/{name}: {} bytes", bytes.len());
}

/// Common ELF64 header prefix with `phnum` program headers at offset 0x40.
fn header(phnum: u16) -> Vec<u8> {
    let mut e = vec![0u8; 0x40];
    e[0..4].copy_from_slice(b"\x7FELF");
    e[4] = 2; // 64-bit
    e[5] = 1; // little-endian
    e[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    e[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    e[24..32].copy_from_slice(&0x8000_0000u64.to_le_bytes()); // entry
    e[32..40].copy_from_slice(&0x40u64.to_le_bytes()); // phoff
    e[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
    e[56..58].copy_from_slice(&phnum.to_le_bytes());
    e
}

fn phdr(flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64) -> [u8; 56] {
    let mut ph = [0u8; 56];
    ph[0..4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    ph[4..8].copy_from_slice(&flags.to_le_bytes());
    ph[8..16].copy_from_slice(&offset.to_le_bytes());
    ph[16..24].copy_from_slice(&vaddr.to_le_bytes());
    ph[32..40].copy_from_slice(&filesz.to_le_bytes());
    ph[40..48].copy_from_slice(&memsz.to_le_bytes());
    ph
}

fn main() {
    println!("seeding loader fuzz corpus:");

    // One R+X segment, 8 file bytes followed by 8 bss bytes.
    let mut one = header(1);
    one.extend_from_slice(&phdr(PF_R | PF_X, 0x78, 0x8000_0000, 8, 16));
    one.resize(0x78, 0);
    one.extend_from_slice(b"codecode");
    write_seed("one_segment", &one);

    // Two PT_LOADs: R+X text then R+W data, each page-aligned per rev0§5.
    let mut two = header(2);
    two.extend_from_slice(&phdr(PF_R | PF_X, 0x1000, 0x8000_0000, 0x20, 0x20));
    two.extend_from_slice(&phdr(PF_R | PF_W, 0x2000, 0x8001_0000, 0x10, 0x40));
    two.resize(0x2010, 0);
    write_seed("two_segments", &two);

    println!("done.");
}
