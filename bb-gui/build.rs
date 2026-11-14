//! Embeds the blackbox logo as the Windows program icon.

fn main() {
    println!("cargo:rerun-if-changed=../packaging/blackbox.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../packaging/blackbox.ico");
        if let Err(e) = res.compile() {
            // A missing resource compiler shouldn't stop the build; the program just has no icon.
            println!("cargo:warning=couldn't embed the icon: {e}");
        }
    }
}
