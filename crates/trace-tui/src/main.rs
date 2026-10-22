use anyhow::{Context, bail};
use clap::builder::{PossibleValue, TypedValueParser};
use clap::{Parser, Subcommand, ValueEnum};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use trace_core::{SPEEDS, SessionId, SessionStatus};
use trace_storage::{SessionFilter, Store};
use trace_tui::app::{App, wall_ns};
use trace_tui::config::Config;

const STALE_AFTER: Duration = Duration::from_secs(300);
const DAY_NS: i64 = 86_400_000_000_000;

#[derive(Parser)]
#[command(
    name = "trace-tui",
    version,
    about = "Terminal trace waterfall for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sessions overview filtered to live runs.
    Live,
    /// Open a session's waterfall. Accepts a unique id prefix.
    Open { id: String },
    /// Replay a recorded session on a virtual clock.
    Replay {
        id: String,
        #[arg(long, default_value = "1", value_parser = speed_parser())]
        speed: f64,
        #[arg(long, value_enum, default_value_t = At::Start)]
        at: At,
    },
    /// Import a JSONL file into the store.
    Import {
        file: PathBuf,
        #[arg(long, default_value = "canonical")]
        adapter: String,
    },
    /// Write a session's normalized journal.
    Export {
        id: String,
        #[arg(long, value_enum, default_value_t = Format::Jsonl)]
        format: Format,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        include_payloads: bool,
    },
    /// Background ingestion process that keeps recording while the TUI is closed
    Collector {
        #[command(subcommand)]
        cmd: CollectorCmd,
    },
    /// Verify socket, storage and adapter health.
    Doctor,
    /// Delete sessions and payloads older than the configured retention.
    Purge,
}

#[derive(Subcommand)]
enum CollectorCmd {
    /// Run the ingestion daemon in the foreground until SIGINT/SIGTERM.
    Start {
        /// `adapter=path`, repeatable. Added to the config's `[[tail]]` list.
        #[arg(long, value_parser = parse_tail)]
        tail: Vec<(String, PathBuf)>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum At {
    Start,
    End,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Jsonl,
}

fn speed_parser() -> impl TypedValueParser<Value = f64> {
    clap::builder::PossibleValuesParser::new(
        SPEEDS.map(|s| PossibleValue::new(&*s.to_string().leak())),
    )
    .map(|s| s.parse::<f64>().expect("SPEEDS entries parse as f64"))
}

fn parse_tail(s: &str) -> Result<(String, PathBuf), String> {
    let (a, p) = s.split_once('=').ok_or("expected adapter=path")?;
    if !trace_ingest::ADAPTERS.contains(&a) {
        return Err(format!(
            "unknown adapter {a:?}; known: {}",
            trace_ingest::ADAPTERS.join(", ")
        ));
    }
    Ok((a.to_owned(), PathBuf::from(p)))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;
    let db = cfg.db_path();
    match cli.cmd {
        None => run_tui(&cfg, SessionFilter::default(), |_| Ok(())),
        Some(Cmd::Live) => run_tui(
            &cfg,
            SessionFilter {
                status: Some(SessionStatus::Live),
                ..Default::default()
            },
            |_| Ok(()),
        ),
        Some(Cmd::Open { id }) => run_tui(&cfg, SessionFilter::default(), |app| {
            let id = resolve(&app.store, &id)?;
            app.open(id)
        }),
        Some(Cmd::Replay { id, speed, at }) => run_tui(&cfg, SessionFilter::default(), |app| {
            let id = resolve(&app.store, &id)?;
            app.open_replay(id, speed, matches!(at, At::End))
        }),
        Some(Cmd::Import { file, adapter }) => {
            let mut store = Store::open(&db)?;
            let out = trace_ingest::import_file(&mut store, &file, &adapter)?;
            println!(
                "imported {} events ({} duplicates) into {}",
                out.inserted,
                out.duplicates,
                db.display()
            );
            Ok(())
        }
        Some(Cmd::Export {
            id,
            format: Format::Jsonl,
            output,
            include_payloads,
        }) => {
            if include_payloads {
                bail!(
                    "--include-payloads is not supported yet: payload blobs have no export format. Nothing was written."
                );
            }
            let store = Store::open(&db)?;
            let id = resolve(&store, &id)?;
            let events = store.session_events(&id)?;
            match output {
                Some(p) => trace_adapter_sdk::write_jsonl(
                    std::io::BufWriter::new(
                        std::fs::File::create(&p)
                            .with_context(|| format!("creating {}", p.display()))?,
                    ),
                    &events,
                )?,
                None => trace_adapter_sdk::write_jsonl(std::io::stdout().lock(), &events)?,
            }
            Ok(())
        }
        Some(Cmd::Collector {
            cmd: CollectorCmd::Start { tail },
        }) => {
            let mut all: Vec<(String, PathBuf)> = cfg
                .tail
                .iter()
                .map(|t| (t.adapter.clone(), t.path.clone()))
                .collect();
            all.extend(tail);
            trace_ingest::run_collector(trace_ingest::CollectorConfig {
                socket_path: cfg.socket_path(),
                db_path: db,
                stale_after: STALE_AFTER,
                tail: all,
                capture: cfg.capture.clone(),
            })
        }
        Some(Cmd::Doctor) => {
            let checks = trace_ingest::doctor(&cfg.socket_path(), &db);
            if checks.is_empty() {
                println!("doctor reported no checks");
            }
            for c in &checks {
                println!(
                    "{} {}: {}",
                    if c.ok { "ok  " } else { "FAIL" },
                    c.name,
                    c.detail
                );
            }
            if checks.iter().any(|c| !c.ok) {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Cmd::Purge) => {
            let now = wall_ns();
            let before = |days: Option<u32>| days.map(|d| now - i64::from(d) * DAY_NS);
            let out = Store::open(&db)?.purge(
                before(cfg.retention.metadata_days),
                before(cfg.retention.payload_days),
            )?;
            println!(
                "purged {} sessions and {} payloads",
                out.sessions, out.payloads
            );
            Ok(())
        }
    }
}

/// A full id or a unique prefix of one.
fn resolve(store: &Store, prefix: &str) -> anyhow::Result<SessionId> {
    let hits: Vec<SessionId> = store
        .list_sessions(&SessionFilter::default())?
        .into_iter()
        .map(|s| s.session_id)
        .filter(|id| id.0.starts_with(prefix))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no session matches {prefix:?}"),
        many if many.iter().any(|id| id.0 == prefix) => Ok(SessionId(prefix.to_owned())),
        many => bail!(
            "{prefix:?} is ambiguous: {}",
            many.iter()
                .map(|i| i.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn run_tui(
    cfg: &Config,
    filter: SessionFilter,
    start: impl FnOnce(&mut App) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut app = App::new(Store::open(&cfg.db_path())?, cfg.theme(), filter)?;
    start(&mut app)?;
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    res
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> anyhow::Result<()> {
    while !app.quit {
        terminal.draw(|f| app.render(f))?;
        if let Some(seq) = app.clipboard.take() {
            let mut out = std::io::stdout();
            out.write_all(seq.as_bytes())?;
            out.flush()?;
        }
        let ready = match app.tick_interval() {
            Some(d) => event::poll(d)?,
            None => true,
        };
        if ready
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
        {
            app.handle_key(k);
        }
        app.tick();
    }
    Ok(())
}
