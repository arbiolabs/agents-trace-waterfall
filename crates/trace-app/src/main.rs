mod app;
mod present;
mod theme;

use clap::Parser;
use gpui_kit::*;
use std::path::PathBuf;

gpui_kit::actions!(trace_waterfall, [Quit]);

/// Desktop trace waterfall for coding agents.
#[derive(Parser)]
#[command(name = "trace-waterfall", version)]
pub struct Cli {
    /// Store to read; defaults to $TRACE_WATERFALL_DB, then the config, then the platform default.
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// Open the session with this id or unique id prefix.
    #[arg(long)]
    pub open: Option<String>,
    /// Open the session in replay, playing from the start.
    #[arg(long)]
    pub replay: bool,
    /// Select the first span whose name contains this text.
    #[arg(long)]
    pub select: Option<String>,
    /// Open the detail popover on the selected span.
    #[arg(long)]
    pub popover: bool,
    #[arg(long, value_parser = ["light", "dark"])]
    pub appearance: Option<String>,
    /// Open with the timeline showing this window, in seconds from the trace start: `START,END`.
    #[arg(long, value_parser = parse_window)]
    pub viewport: Option<(f64, f64)>,
    /// Scroll and pan every frame for 300 frames, print frame timing, and quit.
    #[arg(long, hide = true)]
    pub bench_scroll: bool,
    /// Fold and unfold the biggest subtree near the top for 300 frames, print frame timing, and quit.
    #[arg(long, hide = true)]
    pub bench_fold: bool,
    /// Hold every transition at this progress, 0 to 1, for screenshots. With `--select`, folds the
    /// selected span; with `--popover`, holds the popover opening.
    #[arg(long, hide = true)]
    pub freeze_anim: Option<f32>,
}

fn parse_window(v: &str) -> Result<(f64, f64), String> {
    let (a, b) = v.split_once(',').ok_or("expected START,END")?;
    let n = |x: &str| x.trim().parse::<f64>().map_err(|e| e.to_string());
    Ok((n(a)?, n(b)?))
}

/// `TW_WINDOW=x,y,w,h` in logical points.
fn window_bounds() -> Bounds<Pixels> {
    let parsed = std::env::var("TW_WINDOW").ok().and_then(|v| {
        let n: Vec<f32> = v.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        (n.len() == 4).then(|| Bounds::new(point(px(n[0]), px(n[1])), size(px(n[2]), px(n[3]))))
    });
    parsed.unwrap_or_else(|| Bounds::new(point(px(120.), px(80.)), size(px(1440.), px(900.))))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = trace_tui::config::Config::load()?;
    let db = cli.db.clone().unwrap_or_else(|| cfg.db_path());
    let store = trace_storage::Store::open(&db)?;
    let forced_dark = cli.appearance.as_deref().map(|a| a == "dark");
    application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        cx.set_window_appearance(forced_dark.map(|d| if d { WindowAppearance::Dark } else { WindowAppearance::Light }));
        #[cfg(target_os = "macos")]
        cx.set_reduce_motion(objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(window_bounds())),
            titlebar: Some(TitlebarOptions {
                title: Some("Trace Waterfall".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(18.), px(21.))),
            }),
            window_min_size: Some(size(px(960.), px(600.))),
            ..Default::default()
        };
        let opened = open_window(options, cx, |window, cx| cx.new(|cx| app::TraceApp::new(store, cfg.capture.clone(), forced_dark, &cli, window, cx)));
        if let Err(e) = opened {
            eprintln!("trace-waterfall: {e:#}");
            cx.quit();
        }
        cx.activate(true);
    });
    Ok(())
}
