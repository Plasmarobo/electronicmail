//! electronicmail — shared application core.
//!
//! This library crate holds the entire app (UI, worker, storage, networking)
//! so it can be reused by both targets:
//!
//! * the **desktop** binary ([`run_desktop`], called from `main.rs`), and
//! * the **Android** app, which the OS launches through `android_main`.
//!
//! Keeping everything here means the Android build runs the exact same egui UI
//! and logic as the desktop app — only the entry point and a few
//! platform-specific bits (storage location, self-update) differ.

mod app;
mod auth;
mod autoconfig;
mod calendar;
mod config;
mod fonts;
mod format;
mod htmlview;
mod idle;
mod imap_client;
mod model;
mod search;
mod smtp_client;
mod spam;
mod storage;
mod update;
mod worker;

/// Window icon, embedded at compile time (desktop only).
#[cfg(not(target_os = "android"))]
fn load_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../email.ico"))
        .expect("embedded email.ico is a valid image")
        .into_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

/// Desktop entry point: open a native window running the egui UI.
#[cfg(not(target_os = "android"))]
pub fn run_desktop() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("electronicmail")
            .with_icon(load_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "electronicmail",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}

/// Android entry point, invoked by the `android-activity` runtime once the
/// `NativeActivity` starts. Mirrors [`run_desktop`] but feeds the `AndroidApp`
/// handle into the winit event loop and points storage at the app's private
/// internal data directory (supplied by the OS at runtime).
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid;

    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );

    // Persist config + the SQLite store under the app's private internal dir.
    if let Some(dir) = app.internal_data_path() {
        config::set_data_dir(dir);
    }

    let native_options = eframe::NativeOptions {
        event_loop_builder: Some(Box::new(move |builder| {
            builder.with_android_app(app);
        })),
        ..Default::default()
    };

    if let Err(err) = eframe::run_native(
        "electronicmail",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    ) {
        log::error!("eframe exited with error: {err}");
    }
}
