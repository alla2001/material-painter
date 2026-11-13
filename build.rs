//! Builds the vendored xatlas (automatic UV unwrapping, `src/unwrap.rs`).

fn main() {
    println!("cargo:rerun-if-changed=vendor/xatlas");
    cc::Build::new()
        .cpp(true)
        .std("c++14")
        .file("vendor/xatlas/xatlas.cpp")
        .define("XATLAS_C_API", "1")
        .define("NDEBUG", None)
        .warnings(false)
        .opt_level(3)
        .compile("xatlas");
}
