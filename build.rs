use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-changed=support/observe_shell.rs");
    println!("cargo:rerun-if-changed=src/linux_procfs.rs");
    println!("cargo:rerun-if-changed=src/shell_process/linux_observation.rs");
    println!("cargo:rerun-if-changed=src/shell_process/namespace_report.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    // Ship a single dotlink binary. The small, std-only observer runs inside
    // Bubblewrap with the target's loader; it needs no host install or PATH lookup.
    let target = env::var("TARGET").expect("Cargo must supply TARGET");
    let mut rustc = Command::new(env::var_os("RUSTC").expect("Cargo must supply RUSTC"));
    rustc.args(["--edition=2024", "--target", &target]);
    if let Some(linker) = env::var_os("RUSTC_LINKER") {
        let mut flag = std::ffi::OsString::from("linker=");
        flag.push(linker);
        rustc.arg("-C").arg(flag);
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must supply OUT_DIR"))
        .join("observe-shell");
    rustc.args(["-Copt-level=s", "-Cstrip=symbols", "-Cpanic=abort"]);
    // Cargo's target flags carry sysroot, CRT/linker choices and caller lint policy.
    // Split only on Cargo's separator: paths and individual flags may contain spaces.
    if let Some(flags) = env::var_os("CARGO_ENCODED_RUSTFLAGS") {
        let flags = flags.to_str().expect("Cargo rustflags must be UTF-8");
        rustc.args(flags.split('\x1f').filter(|arg| !arg.is_empty()));
    }
    let status = rustc
        .args(["support/observe_shell.rs", "-o"])
        .arg(output)
        .status()
        .unwrap_or_else(|e| panic!("cannot run rustc for {target} shell observer: {e}"));
    assert!(
        status.success(),
        "rustc for {target} shell observer failed: {status}"
    );
}
