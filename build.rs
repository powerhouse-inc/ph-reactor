//! Records the build target so a release can say which binary it is.
//!
//! `std::env::consts` gives arch and OS but not vendor or the libc flavour,
//! and musl vs gnu is exactly the distinction that decides whether a downloaded
//! binary execs or leaves the node with no daemon. Cargo knows the full target
//! at build time; this hands it to the code rather than having the code guess.
fn main() {
    let get = |k: &str| std::env::var(k).unwrap_or_default();
    let env = get("CARGO_CFG_TARGET_ENV");

    println!("cargo:rustc-env=PH_TARGET_ARCH={}", get("CARGO_CFG_TARGET_ARCH"));
    println!("cargo:rustc-env=PH_TARGET_VENDOR={}", get("CARGO_CFG_TARGET_VENDOR"));
    println!("cargo:rustc-env=PH_TARGET_OS={}", get("CARGO_CFG_TARGET_OS"));
    // Targets like x86_64-apple-darwin have no env component; emit nothing
    // rather than a trailing dash that would not match any real triple.
    println!(
        "cargo:rustc-env=PH_TARGET_ENV_SUFFIX={}",
        if env.is_empty() { String::new() } else { format!("-{env}") }
    );
    println!("cargo:rerun-if-changed=build.rs");
}
