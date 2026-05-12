// Empty Rust shim — the value of this crate lives in build.rs, which
// compiles stubs.c and emits a `cargo:rustc-link-lib` directive that
// link the weak stubs into any binary depending on this crate (i.e.
// the pgrx test binary, since this crate is a dev-dependency only).
