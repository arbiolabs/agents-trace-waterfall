use crate::{Capture, CollectorConfig, adapter_by_name};
use anyhow::{Context, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use trace_adapter_sdk::{MAX_LINE_BYTES, PROTOCOL_VERSION, RawEvent, TraceAdapter, system_now_ns};
use trace_core::{COLLECTOR_SOURCE, CanonicalEvent, Ns, SourceId};
use trace_storage::Store;

const RECONCILE_EVERY: Duration = Duration::from_secs(30);
const TAIL_POLL: Duration = Duration::from_millis(500);
const WRITE_QUEUE: usize = 64;
const BATCH: usize = 1000;

type Reply = oneshot::Sender<anyhow::Result<Option<u64>>>;

/// Requests to the single task that owns the `Store`.
enum Cmd {
    /// Replies with the cursor of `source` once the batch has committed.
    Append {
        events: Vec<CanonicalEvent>,
        source: Option<SourceId>,
        reply: Reply,
    },
    Cursor {
        source: SourceId,
        reply: Reply,
    },
    Reconcile,
}

/// [`crate::run_collector`] that also stops when `stop` fires or its sender is dropped.
pub fn run_collector_until(
    cfg: CollectorConfig,
    stop: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    claim_socket(&cfg.socket_path)?;
    let store = Store::open(&cfg.db_path)?;
    let (tx, rx) = mpsc::channel(WRITE_QUEUE);
    let (stale_after, capture) = (cfg.stale_after, cfg.capture.clone());
    let writer = std::thread::spawn(move || write_loop(store, rx, stale_after, capture));
    let stop_tail = Arc::new(AtomicBool::new(false));
    let tail = (!cfg.tail.is_empty()).then(|| {
        let (files, offsets, tx, stop) = (
            cfg.tail.clone(),
            offsets_path(&cfg.db_path),
            tx.clone(),
            stop_tail.clone(),
        );
        std::thread::spawn(move || tail_loop(&files, &offsets, &tx, &stop))
    });
    let rt = tokio::runtime::Runtime::new()?;
    let served = rt.block_on(serve(&cfg, tx, stop));
    drop(rt);
    stop_tail.store(true, Ordering::Relaxed);
    if let Some(t) = tail {
        let _ = t.join();
    }
    let _ = writer.join();
    eprintln!("collector: stopped");
    served
}

/// Removes a socket file nobody answers on. Refuses to touch a live collector or a non-socket file.
fn claim_socket(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            ensure!(
                m.file_type().is_socket(),
                "{} exists and is not a socket",
                path.display()
            );
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                bail!(
                    "another collector is already listening on {}",
                    path.display()
                );
            }
            fs::remove_file(path)?;
            eprintln!("collector: removed stale socket {}", path.display());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("inspecting {}", path.display())),
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

async fn serve(
    cfg: &CollectorConfig,
    tx: mpsc::Sender<Cmd>,
    mut stop: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let listener = UnixListener::bind(&cfg.socket_path)
        .with_context(|| format!("binding {}", cfg.socket_path.display()))?;
    fs::set_permissions(&cfg.socket_path, fs::Permissions::from_mode(0o600))?;
    eprintln!(
        "collector: listening on {}, database {}",
        cfg.socket_path.display(),
        cfg.db_path.display()
    );
    let (mut int, mut term) = (
        signal(SignalKind::interrupt())?,
        signal(SignalKind::terminate())?,
    );
    let mut reconcile = tokio::time::interval_at(
        tokio::time::Instant::now() + RECONCILE_EVERY,
        RECONCILE_EVERY,
    );
    loop {
        tokio::select! {
            r = listener.accept() => match r {
                Ok((stream, _)) => {
                    tokio::spawn(handle(stream, tx.clone()));
                }
                Err(e) => eprintln!("collector: accept: {e}"),
            },
            _ = reconcile.tick() => match tx.try_send(Cmd::Reconcile) {
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    eprintln!("collector: writer stopped; shutting down");
                    break;
                }
                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            },
            _ = int.recv() => break,
            _ = term.recv() => break,
            _ = &mut stop => break,
        }
    }
    let _ = fs::remove_file(&cfg.socket_path);
    Ok(())
}

fn write_loop(mut store: Store, mut rx: mpsc::Receiver<Cmd>, stale_after: Duration, capture: Capture) {
    reconcile(&mut store, stale_after);
    let mut projects = HashMap::new();
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Cmd::Append {
                mut events,
                source,
                reply,
            } => {
                capture.strip(&mut projects, &mut events);
                let r = store
                    .append(&events)
                    .and_then(|_| source.map_or(Ok(None), |s| store.source_cursor(&s)));
                if let Err(e) = &r {
                    eprintln!(
                        "collector: write failed, {} events not acked: {e:#}",
                        events.len()
                    );
                }
                let _ = reply.send(r);
            }
            Cmd::Cursor { source, reply } => {
                let _ = reply.send(store.source_cursor(&source));
            }
            Cmd::Reconcile => reconcile(&mut store, stale_after),
        }
    }
}

fn reconcile(store: &mut Store, stale_after: Duration) {
    let now = system_now_ns();
    match store.reconcile_interrupted(now - stale_after.as_nanos() as Ns, now) {
        Ok(ids) if !ids.is_empty() => eprintln!(
            "collector: marked {} silent sessions interrupted",
            ids.len()
        ),
        Ok(_) => {}
        Err(e) => eprintln!("collector: reconcile failed: {e:#}"),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg {
    Hello { protocol: u64, source_id: SourceId },
    Events { events: Vec<CanonicalEvent> },
}

async fn handle(stream: tokio::net::UnixStream, tx: mpsc::Sender<Cmd>) {
    let (r, mut w) = stream.into_split();
    if let Err(e) = converse(&mut BufReader::new(r), &mut w, &tx).await {
        let _ = send(
            &mut w,
            json!({"type": "error", "message": format!("{e:#}")}),
        )
        .await;
    }
}

async fn converse(
    r: &mut BufReader<OwnedReadHalf>,
    w: &mut OwnedWriteHalf,
    tx: &mpsc::Sender<Cmd>,
) -> anyhow::Result<()> {
    let Some(msg) = read_msg(r).await? else {
        return Ok(());
    };
    let Msg::Hello {
        protocol,
        source_id,
    } = msg
    else {
        bail!("expected hello")
    };
    ensure!(
        protocol == PROTOCOL_VERSION,
        "unsupported protocol {protocol}; this collector speaks {PROTOCOL_VERSION}"
    );
    ensure!(
        source_id.0 != COLLECTOR_SOURCE,
        "source_id {COLLECTOR_SOURCE:?} is reserved for the collector"
    );
    let cursor = ask(tx, |reply| Cmd::Cursor {
        source: source_id.clone(),
        reply,
    })
    .await?;
    send(
        w,
        json!({"type": "welcome", "protocol": PROTOCOL_VERSION, "cursor": cursor}),
    )
    .await?;
    while let Some(msg) = read_msg(r).await? {
        let Msg::Events { events } = msg else {
            bail!("unexpected second hello")
        };
        events.iter().try_for_each(CanonicalEvent::validate).map_err(anyhow::Error::msg)?;
        let cursor = ask(tx, |reply| Cmd::Append {
            events,
            source: Some(source_id.clone()),
            reply,
        })
        .await
        .context("write failed, not acked")?;
        send(w, json!({"type": "ack", "cursor": cursor})).await?;
    }
    Ok(())
}

/// One NDJSON message, reading at most `MAX_LINE_BYTES`. `None` at a clean end of stream.
async fn read_msg(r: &mut BufReader<OwnedReadHalf>) -> anyhow::Result<Option<Msg>> {
    let mut buf = Vec::new();
    (&mut *r)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if buf.is_empty() {
        return Ok(None);
    }
    ensure!(
        buf.len() <= MAX_LINE_BYTES,
        "line exceeds {MAX_LINE_BYTES} bytes"
    );
    ensure!(buf.ends_with(b"\n"), "connection closed mid-line");
    Ok(Some(serde_json::from_slice(&buf)?))
}

async fn send(w: &mut OwnedWriteHalf, msg: serde_json::Value) -> std::io::Result<()> {
    w.write_all(format!("{msg}\n").as_bytes()).await
}

async fn ask(
    tx: &mpsc::Sender<Cmd>,
    cmd: impl FnOnce(Reply) -> Cmd,
) -> anyhow::Result<Option<u64>> {
    let (reply, rx) = oneshot::channel();
    tx.send(cmd(reply))
        .await
        .map_err(|_| anyhow!("collector is shutting down"))?;
    rx.await.context("collector writer stopped")?
}

struct Tailed {
    adapter: Box<dyn TraceAdapter>,
    /// Bytes fed through `adapter`, always at a line boundary.
    consumed: u64,
    /// Events from lines ending at or before this offset were persisted by an earlier run;
    /// they are replayed through the adapter to rebuild its state but not appended again.
    emit_from: u64,
}

fn offsets_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(".tail-offsets.json");
    p.into()
}

fn tail_loop(
    roots: &[(String, PathBuf)],
    offsets_path: &Path,
    tx: &mpsc::Sender<Cmd>,
    stop: &AtomicBool,
) {
    // A missing or corrupt offsets file only costs a re-read: event ids are deterministic, so the store dedups.
    let mut offsets: BTreeMap<PathBuf, u64> = fs::read(offsets_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut files = HashMap::new();
    while !stop.load(Ordering::Relaxed) {
        for (adapter, root) in roots {
            for path in jsonl_files(root) {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                match tail_file(&path, adapter, &mut files, &mut offsets, tx) {
                    Ok(false) => {}
                    Ok(true) => {
                        if let Err(e) = write_private(
                            offsets_path,
                            &serde_json::to_vec(&offsets).expect("map of paths serializes"),
                        ) {
                            eprintln!("collector: saving {}: {e}", offsets_path.display());
                        }
                    }
                    Err(e) => {
                        eprintln!("collector: tailing {}: {e:#}", path.display());
                        files.remove(&path);
                    }
                }
            }
        }
        std::thread::sleep(TAIL_POLL);
    }
}

/// Feeds new complete lines of `path` through its adapter and appends the events. True if the offset moved.
fn tail_file(
    path: &Path,
    adapter: &str,
    files: &mut HashMap<PathBuf, Tailed>,
    offsets: &mut BTreeMap<PathBuf, u64>,
    tx: &mpsc::Sender<Cmd>,
) -> anyhow::Result<bool> {
    let size = fs::metadata(path)?.len();
    if files.get(path).is_some_and(|t| size < t.consumed) {
        files.remove(path);
        offsets.remove(path);
    }
    if !files.contains_key(path) {
        let done = offsets.get(path).copied().unwrap_or(0);
        if done == size {
            return Ok(false);
        }
        let adapter =
            adapter_by_name(adapter).with_context(|| format!("unknown adapter {adapter:?}"))?;
        files.insert(
            path.to_owned(),
            Tailed {
                adapter,
                consumed: 0,
                emit_from: if done < size { done } else { 0 },
            },
        );
    }
    let t = files.get_mut(path).expect("inserted above");
    if t.consumed == size {
        return Ok(false);
    }
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(t.consumed))?;
    let mut buf = Vec::new();
    f.take(size - t.consumed).read_to_end(&mut buf)?;
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(false);
    };
    let mut pos = t.consumed;
    let mut events = Vec::new();
    for line in buf[..=end].split_inclusive(|&b| b == b'\n') {
        pos += line.len() as u64;
        let raw = RawEvent {
            line: String::from_utf8_lossy(line).trim_end().to_owned(),
            observed_at_utc_ns: system_now_ns(),
        };
        match t.adapter.normalize(raw) {
            Ok(evs) if pos > t.emit_from => events.extend(evs),
            Ok(_) => {}
            Err(e) => eprintln!(
                "collector: {} line ending at byte {pos}: {e:#}",
                path.display()
            ),
        }
    }
    for chunk in events.chunks(BATCH) {
        let (reply, rx) = oneshot::channel();
        tx.blocking_send(Cmd::Append {
            events: chunk.to_vec(),
            source: None,
            reply,
        })
        .map_err(|_| anyhow!("writer stopped"))?;
        rx.blocking_recv().context("writer stopped")??;
    }
    t.consumed = pos;
    offsets.insert(path.to_owned(), pos);
    Ok(true)
}

/// `root` itself if it is a file, else every `*.jsonl` below it, sorted.
fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_owned()];
    }
    let mut out = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(p),
                Ok(t) if t.is_file() && p.extension().is_some_and(|e| e == "jsonl") => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}
