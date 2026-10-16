fn main() {
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
