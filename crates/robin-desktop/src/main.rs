#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_assets;
mod capture;
mod engine;
mod preferences;
mod theme;
mod tray;
mod view;

use gpui_kit::*;
use robin_core::control::Identity;

fn main() -> anyhow::Result<()> {
    let data = std::path::PathBuf::from(std::env::var("LOCALAPPDATA")?).join("Robin");
    let identity = Identity::load(&data.join("identity.key"))?;
    if std::env::args().any(|arg| arg == "--capture-smoke") {
        return capture::smoke();
    }
    let engine = engine::DesktopEngine::new(identity, data.join("disconnected.json"))?;
    let preferences_path = data.join("preferences.json");
    let background = std::env::args().any(|arg| arg == "--background");
    gpui_kit::application()
        .with_assets(app_assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::install(cx);
            let (handle, view) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(480.), px(760.)),
                        cx,
                    ))),
                    window_min_size: Some(size(px(480.), px(640.))),
                    is_resizable: false,
                    titlebar: Some(TitlebarOptions {
                        title: Some("知更鸟 · Robin".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    cx.new(|cx| view::RobinView::new(engine, preferences_path, window, cx))
                },
            )
            .expect("could not open Robin window");
            if background && view.read(cx).has_tray() {
                let _ = handle.update(cx, |_, window, _| tray::hide(window));
            } else {
                cx.activate(true);
            }
        });
    Ok(())
}
