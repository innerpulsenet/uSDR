//! On Windows, embed the app icon in the executable so Explorer and the
//! taskbar show it. Nothing to do elsewhere: Linux finds the icon through the
//! launcher (packaging/linux).

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/usdr.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon/usdr.ico");
        res.set("ProductName", "uSDR");
        res.set("FileDescription", "uSDR desktop receiver console");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the icon: {e}");
        }
    }
}
