//! Embeds the dog-face icon and version metadata into the Windows executable.
//!
//! The window icon set through eframe only affects the title bar/taskbar at
//! runtime; Explorer reads the icon from the PE resources. Both must point at
//! the same artwork.

fn main() {
    println!("cargo:rerun-if-changed=crates/pml-ui/assets/dog_face.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("crates/pml-ui/assets/dog_face.ico");
    resource.set("ProductName", pml_brand());
    resource.set("FileDescription", pml_brand());
    resource.set("CompanyName", "Odium Launcher contributors");
    resource.set("LegalCopyright", "0BSD licensed");
    resource.set("OriginalFilename", "OdiumLauncher.exe");
    if let Err(error) = resource.compile() {
        // A missing Windows SDK resource compiler must not break the build on
        // developer machines that only need `cargo check`.
        println!("cargo:warning=could not embed the Windows icon: {error}");
    }
}

fn pml_brand() -> &'static str {
    match option_env!("ODIUM_BRAND_NAME") {
        Some(name) => name,
        None => "Odium Launcher",
    }
}
