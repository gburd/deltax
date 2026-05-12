// Compile weak stubs for Postgres backend symbols and emit a link directive
// so anything depending on this crate (just the pgrx unit-test binary, since
// this is a dev-dependency) picks them up. The cdylib never links this crate
// — `cargo build --lib` (what `cargo pgrx install` runs) skips dev-deps — so
// Postgres still gets the real symbols at extension load time.
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" {
        return;
    }

    println!("cargo:rerun-if-changed=stubs.c");

    cc::Build::new()
        .file("stubs.c")
        .flag_if_supported("-fPIC")
        .compile("pg_deltax_test_stubs");
}
