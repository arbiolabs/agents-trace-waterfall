use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use std::path::PathBuf;
use std::time::Duration;
use trace_core::{CanonicalEvent, Replay, SessionId, SpanId};
use trace_render::Theme;
use trace_storage::{SessionFilter, Store};
use trace_tui::app::{App, Tab};
use trace_tui::ui::inspector_lines;
use trace_view::{Mode, RowKind, TraceDoc};

const SESSION: &str = "01KDVDNA000000000000000001";

fn sample() -> Vec<CanonicalEvent> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/sample-trace.jsonl"
    );
    trace_adapter_sdk::read_jsonl(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
        .unwrap()
}

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("trace-tui-{}-{name}.db", std::process::id()));
        Self(p).cleaned()
    }
    fn cleaned(self) -> Self {
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
        }
        self
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
        }
    }
}

fn app_with(store: Store) -> App {
    App::new(store, Theme::default(), SessionFilter::default()).unwrap()
}

fn opened(events: &[CanonicalEvent]) -> App {
    let mut store = Store::open_in_memory().unwrap();
    store.append(events).unwrap();
    let mut app = app_with(store);
    app.open(SessionId::from(SESSION)).unwrap();
    app
}

/// App reading `db`, with the first `n` sample events written by a separate connection (the collector's role).
fn live(db: &TempDb, n: usize) -> (App, Store) {
    let mut writer = Store::open(&db.0).unwrap();
    writer.append(&sample()[..n]).unwrap();
    let mut app = app_with(Store::open(&db.0).unwrap());
    app.open(SessionId::from(SESSION)).unwrap();
    (app, writer)
}

fn press(app: &mut App, keys: &str) {
    for c in keys.chars() {
        app.handle_key(KeyEvent::from(KeyCode::Char(c)));
    }
}

fn key(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::from(code));
}

fn doc(app: &App) -> &TraceDoc {
    &app.view.as_ref().unwrap().doc
}

fn selected(app: &App) -> String {
    doc(app).selected_span().unwrap().name.clone()
}

fn row_names(app: &App) -> Vec<String> {
    let v = doc(app);
    v.rows
        .iter()
        .map(|r| match &r.kind {
            RowKind::Span(id) => v.state.spans[id].name.clone(),
            RowKind::UnresolvedHeader => "Unresolved".into(),
        })
        .collect()
}

fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.render(f)).unwrap();
    let buf = term.backend().buffer();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buf[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

fn span_id_named(app: &App, name: &str) -> SpanId {
    doc(app)
        .state
        .spans
        .values()
        .find(|s| s.name == name)
        .unwrap()
        .span_id
        .clone()
}

#[test]
fn navigates_tree_with_jkhl_and_g() {
    let mut app = opened(&sample());
    assert_eq!(row_names(&app).len(), 21);
    assert_eq!(selected(&app), "agent");
    press(&mut app, "j");
    assert_eq!(selected(&app), "turn 1");
    press(&mut app, "l");
    assert_eq!(selected(&app), "llm call");
    press(&mut app, "h");
    assert_eq!(selected(&app), "turn 1");
    press(&mut app, "h");
    assert_eq!(
        row_names(&app).len(),
        19,
        "turn 1 collapsed hides its two children"
    );
    press(&mut app, "l");
    assert_eq!(row_names(&app).len(), 21);
    press(&mut app, "G");
    assert_eq!(selected(&app), "write_file");
    press(&mut app, "gk");
    assert_eq!(selected(&app), "agent");
    press(&mut app, "e");
    assert_eq!(row_names(&app), ["agent"], "e collapses every descendant");
    press(&mut app, "e");
    assert_eq!(row_names(&app).len(), 21);
}

#[test]
fn collapsed_branch_stays_collapsed_when_live_events_arrive() {
    let db = TempDb::new("collapse");
    let (mut app, mut writer) = live(&db, 13);
    assert!(matches!(doc(&app).mode, Mode::Live { .. }));
    press(&mut app, "jh");
    assert_eq!(row_names(&app), ["agent", "turn 1", "turn 2"]);
    writer.append(&sample()[13..30]).unwrap();
    app.tick_by(Duration::ZERO).unwrap();
    assert_eq!(
        row_names(&app),
        [
            "agent",
            "turn 1",
            "turn 2",
            "llm call",
            "edit_file",
            "turn 3",
            "llm call"
        ]
    );
    assert_eq!(selected(&app), "turn 1");
}

#[test]
fn selection_follows_span_id_when_a_child_arrives_above() {
    let db = TempDb::new("stable");
    let (mut app, mut writer) = live(&db, 13);
    press(&mut app, "jjjj");
    assert_eq!(selected(&app), "turn 2");
    assert_eq!(doc(&app).selected_row, 4);
    let late: CanonicalEvent = serde_json::from_value(serde_json::json!({
        "source_id": "late-source", "source_seq": 1, "event_id": "late-1",
        "observed_at_utc_ns": 1767225601000000000i64, "occurred_at_utc_ns": 1767225601000000000i64,
        "session_id": SESSION, "span_id": "late-span", "event_type": "span.started",
        "body": {"parent_span_id": "01KDVDNA00X0P9RZXGBW000004", "name": "late tool", "kind": "tool"}
    }))
    .unwrap();
    writer.append(&[late]).unwrap();
    app.tick_by(Duration::ZERO).unwrap();
    assert_eq!(row_names(&app)[4], "late tool");
    assert_eq!(selected(&app), "turn 2");
    assert_eq!(doc(&app).selected_row, 5);
}

#[test]
fn pause_buffers_and_resume_applies_every_event() {
    let db = TempDb::new("pause");
    let (mut app, mut writer) = live(&db, 20);
    press(&mut app, " ");
    writer.append(&sample()[20..]).unwrap();
    app.tick_by(Duration::ZERO).unwrap();
    let v = doc(&app);
    assert_eq!(v.state.event_count, 20);
    assert!(
        v.status().starts_with("PAUSED follow lag "),
        "{}",
        v.status()
    );
    assert!(v.status().ends_with("queued 50"), "{}", v.status());
    press(&mut app, " ");
    let v = doc(&app);
    assert_eq!(
        v.state.event_count,
        writer
            .session_events(&SessionId::from(SESSION))
            .unwrap()
            .len()
    );
    assert_eq!(v.state.event_count, 70);
    assert!(
        matches!(v.mode, Mode::Static),
        "session.ended moves a live view to static"
    );
}

#[test]
fn replay_at_t_matches_replay_state_at() {
    let mut store = Store::open_in_memory().unwrap();
    store.append(&sample()).unwrap();
    let mut app = app_with(store);
    let id = SessionId::from(SESSION);
    app.open_replay(id.clone(), 1.0, false).unwrap();
    press(&mut app, " ]]]");
    let reference = Replay::new(id, sample());
    let start = reference.start().unwrap();
    let Mode::Replay { clock, .. } = &doc(&app).mode else {
        panic!("not replay")
    };
    let t = clock.position;
    assert_eq!(t - start, 528_000_000);
    assert!(!clock.playing);
    let v = doc(&app);
    assert_eq!(v.state, reference.state_at(t));
    assert_eq!(v.state.event_count, 6);
    assert_eq!(row_names(&app), ["agent", "turn 1", "llm call"]);

    press(&mut app, "[");
    let Mode::Replay { clock, .. } = &doc(&app).mode else {
        panic!()
    };
    assert_eq!(clock.position - start, 394_000_000);

    press(&mut app, ">> ");
    app.tick_by(Duration::from_millis(100)).unwrap();
    let Mode::Replay { clock, .. } = &doc(&app).mode else {
        panic!()
    };
    assert_eq!(clock.speed(), 5.0);
    assert_eq!(clock.position - start, 894_000_000);
    assert_eq!(doc(&app).state, reference.state_at(clock.position));
}

#[test]
fn renders_at_supported_sizes_with_footer() {
    let mut app = opened(&sample());
    for (w, h) in [(60, 18), (100, 30), (160, 45)] {
        app.screen = trace_tui::app::Screen::Waterfall;
        let s = screen(&mut app, w, h);
        assert!(
            s[h as usize - 1].starts_with("STATIC │ j/k h/l"),
            "{w}x{h}: {:?}",
            s.last()
        );
        assert!(
            s.iter().any(|l| l.contains("turn 1")),
            "{w}x{h} shows span names"
        );
        assert!(
            s[0].starts_with("completed wall 16.2s"),
            "{w}x{h}: {}",
            s[0]
        );

        key(&mut app, KeyCode::Enter);
        let s = screen(&mut app, w, h);
        assert!(
            s[h as usize - 1].starts_with("STATIC │ h/l tab"),
            "{w}x{h}: {:?}",
            s.last()
        );
        assert!(
            s.iter().any(|l| l.contains("Overview")),
            "{w}x{h} shows inspector"
        );
        key(&mut app, KeyCode::Esc);

        press(&mut app, "?");
        let s = screen(&mut app, w, h);
        assert!(s[h as usize - 1].starts_with("j/k scroll · Esc/? back"));
        key(&mut app, KeyCode::Esc);

        press(&mut app, "s");
        let s = screen(&mut app, w, h);
        assert!(s[h as usize - 1].starts_with("[all] j/k move"));
        assert!(
            s.iter().any(|l| l.contains("01KDVDNA00")),
            "{w}x{h} lists the session: {s:#?}"
        );
        key(&mut app, KeyCode::Enter);
    }
}

#[test]
fn narrow_layout_tabs_between_full_width_views() {
    let mut app = opened(&sample());
    let s = screen(&mut app, 60, 18);
    assert!(
        s[3].contains("turn 1") && s[3].contains("1.7s"),
        "tree view: {}",
        s[3]
    );
    key(&mut app, KeyCode::Tab);
    let s = screen(&mut app, 60, 18);
    assert!(s[s.len() - 1].starts_with("STATIC │ h/l pan"));
    key(&mut app, KeyCode::Tab);
    let s = screen(&mut app, 60, 18);
    assert!(s[1].contains("Overview"), "{}", s[1]);
    assert!(s.iter().any(|l| l.contains("children  5")));
}

#[test]
fn search_status_failed_keeps_failed_span_and_ancestors() {
    let mut app = opened(&sample());
    press(&mut app, "/status:failed");
    key(&mut app, KeyCode::Enter);
    assert_eq!(row_names(&app), ["agent", "turn 3", "bash"]);
    press(&mut app, "G");
    assert_eq!(
        doc(&app).selected_span().unwrap().status,
        trace_core::SpanStatus::Failed
    );
    press(&mut app, "/");
    for _ in 0.."status:failed".len() {
        key(&mut app, KeyCode::Backspace);
    }
    press(&mut app, "kind:tool read");
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        row_names(&app),
        [
            "agent",
            "turn 1",
            "read_file",
            "turn 4",
            "explore codebase",
            "read_file"
        ]
    );
    key(&mut app, KeyCode::Esc);
    assert_eq!(row_names(&app).len(), 21);
}

#[test]
fn inspector_says_not_captured_for_missing_output() {
    let mut app = opened(&sample());
    press(&mut app, "jj");
    assert_eq!(selected(&app), "llm call");
    key(&mut app, KeyCode::Enter);
    press(&mut app, "ll");
    let v = app.view.as_ref().unwrap();
    assert_eq!(v.inspector.unwrap().tab, Tab::Output);
    let lines: Vec<String> = inspector_lines(&v.doc, Tab::Output)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(lines, ["not captured"]);
    assert!(
        screen(&mut app, 160, 45)
            .iter()
            .any(|l| l.contains("not captured"))
    );
    let failed = span_id_named(&app, "bash");
    let v = &mut app.view.as_mut().unwrap().doc;
    v.selected = Some(failed);
    v.refresh();
    let err: Vec<String> = inspector_lines(doc(&app), Tab::Error)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(err, ["exit status 1: 2 tests failed"]);
}

#[test]
fn events_tab_saturates_extreme_offsets() {
    let mut app = opened(&sample());
    press(&mut app, "jj");
    let v = &mut app.view.as_mut().unwrap().doc;
    let id = v.selected.clone().unwrap();
    let s = v.state.spans.get_mut(&id).unwrap();
    s.started_at_utc_ns = i64::MAX;
    s.events[0].timestamp_utc_ns = i64::MIN;
    let lines: Vec<String> = inspector_lines(doc(&app), Tab::Events)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(lines, ["+-2562047h47m first_token"]);
}

#[test]
fn span_export_sanitizes_the_id_and_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let app = opened(&sample());
    let mut span = doc(&app).selected_span().unwrap().clone();
    span.span_id = SpanId::from("../x/y");
    let dir = std::env::temp_dir().join(format!("trace-tui-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = trace_view::export_span(&dir, &span).unwrap();
    assert_eq!(path, dir.join("span-.._x_y.json"));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(back["span_id"], "../x/y");
    std::fs::remove_dir_all(&dir).unwrap();
}
