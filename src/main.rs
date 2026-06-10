//! electronicmail — a lean, fast email client.

mod app;
mod auth;
mod autoconfig;
mod calendar;
mod config;
mod fonts;
mod htmlview;
mod idle;
mod imap_client;
mod model;
mod search;
mod smtp_client;
mod spam;
mod storage;
mod worker;

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

fn main() -> eframe::Result<()> {
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
