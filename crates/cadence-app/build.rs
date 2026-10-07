//! LLD 20+ treats a PIE with no shared inputs as static and resolves ignored
//! undefined symbols to zero, even when it has PT_INTERP. A symbol-free, as-needed
//! DSO input enables proper dynamic imports without creating a DT_NEEDED entry.
//! See llvm/llvm-project commit 994cea3f0a2d0caf4d66321ad5a06ab330144d89.
//!
//! This is a LINK-TIME ELF symbol-table container, not executable code, not a
//! library to install, and not another copy of Scarlet's loader. Every native
//! executable must still audit its final dynamic imports/relocations.
use std::{env, fs, path::PathBuf};

fn put16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Match the catalog identity without maintaining another descriptor or ID.
    let recipe =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
            .join("../../platforms/scarlet/app.toml");
    println!("cargo:rerun-if-changed={}", recipe.display());
    let metadata: toml::Value = toml::from_str(&fs::read_to_string(recipe).expect("read app.toml"))
        .expect("parse app.toml");
    for (key, variable) in [("id", "CADENCE_APP_ID"), ("name", "CADENCE_APP_NAME")] {
        let value = metadata
            .get("app")
            .and_then(|app| app.get(key))
            .and_then(toml::Value::as_str)
            .expect("app.toml requires app.id and app.name");
        assert!(
            !value.is_empty() && !value.chars().any(char::is_control),
            "invalid app metadata"
        );
        println!("cargo:rustc-env={variable}={value}");
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("scarlet") {
        return;
    }
    let machine = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => 183,
        Ok("riscv64") => 243,
        _ => panic!("Cadence on Scarlet supports only AArch64 and RISC-V64"),
    };
    let mut bytes = [0u8; 281];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[7] = 83; // ELFOSABI_SCARLET
    put16(&mut bytes, 16, 3); // ET_DYN
    put16(&mut bytes, 18, machine);
    put32(&mut bytes, 20, 1); // EV_CURRENT
    put64(&mut bytes, 40, 64); // section table follows ELF header
    put16(&mut bytes, 52, 64); // ELF64 header size
    put16(&mut bytes, 58, 64); // section header size
    put16(&mut bytes, 60, 3); // null, dynsym, dynstr
    // Section 1: dynamic symbol table containing the required null symbol only.
    put32(&mut bytes, 128 + 4, 11); // SHT_DYNSYM
    put64(&mut bytes, 128 + 8, 2); // SHF_ALLOC
    put64(&mut bytes, 128 + 24, 256);
    put64(&mut bytes, 128 + 32, 24);
    put32(&mut bytes, 128 + 40, 2); // linked string table
    put32(&mut bytes, 128 + 44, 1); // first non-local symbol index
    put64(&mut bytes, 128 + 48, 8);
    put64(&mut bytes, 128 + 56, 24);
    // Section 2: one empty string. There are no definitions or imports.
    put32(&mut bytes, 192 + 4, 3); // SHT_STRTAB
    put64(&mut bytes, 192 + 8, 2);
    put64(&mut bytes, 192 + 24, 280);
    put64(&mut bytes, 192 + 32, 1);
    put64(&mut bytes, 192 + 48, 1);
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    fs::write(out.join("libcadence_link_seed.so"), bytes).expect("write link-only ELF");
    println!("cargo:rustc-link-search=native={}", out.display());
    // rustc passes native dylibs under --as-needed. The final ELF audit must
    // require no DT_NEEDED entries, proving this input was discarded.
    println!("cargo:rustc-link-lib=dylib=cadence_link_seed");
    // SGFX and CLAP use the resident loader's four dl* APIs.
    // scripts/audit-scarlet.py must pass before deploying every native binary.
    for argument in [
        "-pie",
        "--dynamic-linker=/bin/scarlet-ld",
        "--export-dynamic",
        "--unresolved-symbols=ignore-all",
        "-z",
        "now",
    ] {
        println!("cargo:rustc-link-arg-bin=cadence={argument}");
    }
}
