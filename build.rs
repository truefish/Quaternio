fn main() {
    // The generated desktop UI can exceed the Windows main-thread stack in
    // debug builds. RUST_MIN_STACK only affects newly spawned Rust threads.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=brand/quaternio.rc");
        println!("cargo:rerun-if-changed=brand/quaternio.ico");
        embed_resource::compile_for("brand/quaternio.rc", ["quaternio"], embed_resource::NONE)
            .manifest_required()
            .expect("embed Quaternio executable icon");
        let stack = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            "/STACK:16777216"
        } else {
            "-Wl,--stack,16777216"
        };
        println!("cargo:rustc-link-arg-bin=quaternio={stack}");
    }
    // Windows build-script threads have a small default stack. Compiling the
    // docked editor's nested Slint components needs a larger local compiler stack.
    std::thread::Builder::new()
        .name("quaternio-ui-compiler".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| slint_build::compile("ui/app.slint"))
        .expect("start UI compiler")
        .join()
        .expect("UI compiler thread")
        .expect("compile editor UI");
}
