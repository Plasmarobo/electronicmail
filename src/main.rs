//! electronicmail — desktop entry point.
//!
//! All application logic lives in the library crate (see `lib.rs`) so it can be
//! shared with the Android build, which enters through `android_main` instead.

#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result<()> {
    electronicmail::run_desktop()
}

// On Android the entry point is `android_main` in the library crate; the binary
// target is unused there, so `main` is just an empty stub.
#[cfg(target_os = "android")]
fn main() {}
