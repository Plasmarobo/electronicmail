//! electronicmail — desktop entry point.
//!
//! All application logic lives in the library crate (see `lib.rs`) so it can be
//! shared with the Android build, which enters through `android_main` instead.

// In release builds, attach to the Windows GUI subsystem so launching the app
// doesn't pop up an extra console window alongside the egui window. Debug builds
// keep the console so logs and panics stay visible. (Ignored on non-Windows.)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result<()> {
    electronicmail::run_desktop()
}

// On Android the entry point is `android_main` in the library crate; the binary
// target is unused there, so `main` is just an empty stub.
#[cfg(target_os = "android")]
fn main() {}
