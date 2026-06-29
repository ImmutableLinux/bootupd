fn main() {
    println!("cargo::rustc-check-cfg=cfg(efi_arch)");

    if cfg!(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )) {
        println!("cargo:rustc-cfg=efi_arch");
    }
}
