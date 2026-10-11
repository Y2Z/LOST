//! On Windows, puts the icon and version details into lost.exe.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=dist/icons/lost.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::path::Path::new("dist/icons/lost.ico").exists()
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("dist/icons/lost.ico");
        resource.set("FileDescription", "LOST (Local Offline Search Tool)");
        resource.set("ProductName", "LOST");
        if let Err(e) = resource.compile() {
            // An exe without an icon still works.
            println!("cargo:warning=no icon in lost.exe: {e}");
        }
    }
}
