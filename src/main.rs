mod app;
mod platform;
mod settings;
mod source;
mod sync;
mod ui;
mod video;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    log::info!("Starting HiSpeed Viewer");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("HiSpeed Viewer")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "HiSpeed Viewer",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
