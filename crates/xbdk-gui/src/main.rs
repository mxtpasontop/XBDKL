//! `xbdk-gui`: Xbox 360 Neighborhood for Linux, graphical edition.

mod app;
mod worker;

use eframe::egui;

const USAGE: &str = "usage: xbdk-gui [--tab console|files|screen|debug]
The console is taken from XBDK_CONSOLE, or found on the network.";

fn main() -> eframe::Result {
    let mut tab = app::Tab::Console;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next().as_deref().and_then(app::Tab::from_name)) {
            ("--tab", Some(t)) => tab = t,
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Xbox 360 Neighborhood")
            .with_app_id("xbdk-gui")
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("xbdk-gui", options, Box::new(|cc| Ok(Box::new(app::XbdkApp::new(cc, tab)))))
}
