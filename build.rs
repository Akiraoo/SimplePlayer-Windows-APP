fn main() {
    slint_build::compile("ui/app.slint").expect("Slint UI compile failed");

    // Windows: embed the icon and version info into the .exe. Windows media controls
    // (and Task Manager) take the app name from FileDescription; without it they show
    // "Unknown app".
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/app.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico")
            .set("FileDescription", "Simple Player")
            .set("ProductName", "Simple Player")
            .set("InternalName", "SimplePlayer")
            .set("OriginalFilename", "SimplePlayer.exe")
            .set("CompanyName", "Akiraoo")
            .set("LegalCopyright", "Apache-2.0");
        if let Err(e) = res.compile() {
            println!("cargo:warning=Could not embed the Windows icon/version info: {e}");
        }
    }
}
