mod app;
mod present;
mod theme;

use clap::Parser;
use gpui_kit::*;
use std::path::PathBuf;

use app::*;

/// Desktop trace waterfall for coding agents.
#[derive(Parser)]
#[command(name = "agents-trace-waterfall", version)]
pub struct Cli {
    /// Store to read; defaults to $AGENTS_TRACE_WATERFALL_DB, then the config, then the platform default.
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
    /// With `--popover`, scroll the popover's body to its end, for screenshots.
    #[arg(long, hide = true)]
    pub popover_scroll_end: bool,
    /// Hold every transition at this progress, 0 to 1, for screenshots. With `--select`, folds the
    /// selected span; with `--popover`, holds the popover opening.
    #[arg(long, hide = true)]
    pub freeze_anim: Option<f32>,
    /// Press Tab this many times once the window has drawn, for focus ring screenshots.
    #[arg(long, hide = true)]
    pub tab: Option<usize>,
    /// Right-click at `X,Y` in window points once the window has drawn, for context menu screenshots.
    #[arg(long, hide = true, value_parser = parse_window)]
    pub context_menu: Option<(f64, f64)>,
    /// List sessions by recency or by workspace; overrides the remembered choice.
    #[arg(long, hide = true, value_parser = ["recent", "workspaces"])]
    pub sidebar_mode: Option<String>,
    /// Collapse the sidebar workspace or project with this name; repeatable.
    #[arg(long, hide = true)]
    pub collapse: Vec<String>,
    /// Type this into the session search field.
    #[arg(long, hide = true)]
    pub search: Option<String>,
}

fn parse_window(v: &str) -> Result<(f64, f64), String> {
    let (a, b) = v.split_once(',').ok_or("expected START,END")?;
    let n = |x: &str| x.trim().parse::<f64>().map_err(|e| e.to_string());
    Ok((n(a)?, n(b)?))
}

/// Lucide icons plus the brand marks under `brands/`. `brands/x.svg?pt=12` serves the mark sized to
/// 12pt, so `img()`, which rasterizes an SVG at twice its own size, renders it 1:1 on a 2x display.
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        let Some(name) = path.strip_prefix("brands/") else { return gpui_kit::assets::AllAssets.load(path) };
        let (file, pt) = name.split_once("?pt=").map_or((name, None), |(f, p)| (f, p.parse::<u32>().ok()));
        let Some(logo) = file.strip_suffix(".svg").and_then(present::Logo::by_file) else { return Ok(None) };
        Ok(Some(match pt {
            None => logo.svg().into(),
            Some(pt) => String::from_utf8_lossy(logo.svg()).replacen("<svg ", &format!("<svg width=\"{pt}\" height=\"{pt}\" "), 1).into_bytes().into(),
        }))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_kit::assets::AllAssets.list(path)
    }
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
    application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);
        cx.set_window_appearance(forced_dark.map(|d| if d { WindowAppearance::Dark } else { WindowAppearance::Light }));
        #[cfg(target_os = "macos")]
        cx.set_reduce_motion(objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-o", Import, None),
            KeyBinding::new("cmd-e", ExportSession, None),
            KeyBinding::new("cmd-alt-s", ToggleSidebar, None),
            KeyBinding::new("cmd-=", ZoomIn, None),
            KeyBinding::new("cmd-+", ZoomIn, None),
            KeyBinding::new("cmd--", ZoomOut, None),
            KeyBinding::new("cmd-0", ZoomToFit, None),
            KeyBinding::new("cmd-alt-left", CollapseAll, None),
            KeyBinding::new("cmd-alt-right", ExpandAll, None),
            KeyBinding::new("cmd-shift-f", Follow, None),
            KeyBinding::new("cmd-r", Replay, None),
            KeyBinding::new("cmd-]", NextEvent, None),
            KeyBinding::new("cmd-[", PrevEvent, None),
            KeyBinding::new("enter", Inspect, Some(ROWS_CONTEXT)),
            KeyBinding::new("n", AddNote, Some(ROWS_CONTEXT)),
            KeyBinding::new("cmd-c", CopySpanName, Some(ROWS_CONTEXT)),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus([
            Menu::new("Agents Trace Waterfall").items([
                MenuItem::action("About Agents Trace Waterfall", About),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit Agents Trace Waterfall", Quit),
            ]),
            Menu::new("File").items([MenuItem::action("Import JSONL…", Import), MenuItem::action("Export Session…", ExportSession)]),
            Menu::new("View").items([
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::separator(),
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::action("Zoom to Fit", ZoomToFit),
                MenuItem::separator(),
                MenuItem::action("Collapse All Spans", CollapseAll),
                MenuItem::action("Expand All Spans", ExpandAll),
                MenuItem::separator(),
                MenuItem::action("Follow", Follow),
            ]),
            Menu::new("Go").items([
                MenuItem::action("Replay", Replay),
                MenuItem::separator(),
                MenuItem::action("Next Event", NextEvent),
                MenuItem::action("Previous Event", PrevEvent),
            ]),
        ]);
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(window_bounds())),
            titlebar: Some(TitlebarOptions {
                title: Some("Agents Trace Waterfall".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(18.), px(21.))),
            }),
            window_min_size: Some(size(px(960.), px(600.))),
            window_background: WindowBackgroundAppearance::Blurred,
            ..Default::default()
        };
        let opened = open_window(options, cx, |window, cx| cx.new(|cx| app::TraceApp::new(store, cfg.capture.clone(), forced_dark, &cli, db.with_extension("sidebar.json"), window, cx)));
        match opened {
            // The kit's root paints the theme background over the whole window; clear it so the
            // sidebar's translucent tray shows the window's blur.
            Ok((handle, _)) => {
                handle
                    .update(cx, |_, window, cx| {
                        if let Some(Some(root)) = window.root::<gpui_kit::base::Root>() {
                            root.update(cx, |r, _| Styled::style(r).background = Some(transparent_black().into()));
                        }
                    })
                    .ok();
            }
            Err(e) => {
                eprintln!("agents-trace-waterfall: {e:#}");
                cx.quit();
            }
        }
        cx.activate(true);
    });
    Ok(())
}
