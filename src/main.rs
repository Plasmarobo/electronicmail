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

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("electronicmail"),
        ..Default::default()
    };

    eframe::run_native(
        "electronicmail",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
