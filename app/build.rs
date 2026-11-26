fn main() {
    // Embed the application manifest (asInvoker, per-monitor DPI, Common Controls v6).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_manifest_file("app.manifest");
        res.set_icon("assets/app.ico");
        res.set("FileDescription", "Win Manager RS");
        res.set("ProductName", "Win Manager RS");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed resources: {e}");
        }
    }
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=assets/app.ico");
}
