use std::path::{Path, PathBuf};
use std::process::Command;
use trace_core::CanonicalEvent;

const SAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/sample-trace.jsonl"
);

fn read(path: &Path) -> Vec<CanonicalEvent> {
    let mut v =
        trace_adapter_sdk::read_jsonl(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
            .unwrap();
    v.sort_by_key(CanonicalEvent::dedup_key);
    v
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trace-tui-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tui(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_trace-tui"))
        .args(args)
        .env("TRACE_WATERFALL_DB", dir.join("t.db"))
        .env("TRACE_WATERFALL_CONFIG", dir.join("missing.toml"))
        .output()
        .unwrap()
}

#[test]
fn export_by_prefix_writes_the_stored_journal() {
    let dir = tmp("export");
    trace_storage::Store::open(&dir.join("t.db"))
        .unwrap()
        .append(&read(Path::new(SAMPLE)))
        .unwrap();
    let out = dir.join("out.jsonl");
    let r = tui(&dir, &["export", "01KDVDNA", "-o", out.to_str().unwrap()]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(read(&out), read(Path::new(SAMPLE)));

    let r = tui(
        &dir,
        &[
            "export",
            "01KDVDNA",
            "--include-payloads",
            "-o",
            dir.join("p.jsonl").to_str().unwrap(),
        ],
    );
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("--include-payloads is not supported yet"));
    assert!(!dir.join("p.jsonl").exists());

    let r = tui(&dir, &["export", "nope"]);
    assert!(String::from_utf8_lossy(&r.stderr).contains("no session matches \"nope\""));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "needs trace_ingest::import_file, which is a stub on this branch"]
fn import_then_export_round_trips() {
    let dir = tmp("roundtrip");
    let r = tui(&dir, &["import", SAMPLE]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let out = dir.join("out.jsonl");
    let r = tui(
        &dir,
        &[
            "export",
            "01KDVDNA000000000000000001",
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(read(&out), read(Path::new(SAMPLE)));
    std::fs::remove_dir_all(dir).unwrap();
}
