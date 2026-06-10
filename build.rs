fn main() {
    // Embed the application icon into the Windows executable so it shows up in
    // Explorer, the taskbar and the title bar. No-op on other platforms.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=email.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("email.ico");
        if let Err(err) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {err}");
        }
    }
}
