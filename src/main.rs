mod app;
mod chat;
mod claude;
mod config;
mod editor;
mod format;
mod git;
mod highlight;
mod lsp;
mod md;
mod palette;
mod search;
mod text;
mod theme;
mod tree;
mod vim;
#[cfg(test)]
#[path = "validate/test_env.rs"]
mod test_env;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let arg = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([480.0, 320.0])
            .with_title("rim")
            .with_app_id("rim"),
        ..Default::default()
    };
    eframe::run_native("rim", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, arg)))))
}

