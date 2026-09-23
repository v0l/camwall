mod app;
mod config;
mod frigate;
mod power;
mod stats;
mod workers;

use eframe::egui;

fn main() -> eframe::Result {
    let cfg = match config::Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("camwall: {e}");
            std::process::exit(2);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("camwall")
            .with_app_id("camwall")
            .with_fullscreen(true)
            .with_decorations(false),
        renderer: eframe::Renderer::Glow,
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "camwall",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, cfg)))),
    )
}
