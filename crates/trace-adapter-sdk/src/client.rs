use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use trace_core::{CanonicalEvent, SourceId};

pub const PROTOCOL_VERSION: u64 = 1;
pub const MAX_LINE_BYTES: usize = 1 << 20;

const FRAME_HEAD: &str = r#"{"type":"events","events":["#;
const FRAME_TAIL: &str = "]}\n";
const MIN_BACKOFF: Duration = Duration::from_millis(50);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Blocking collector client for one source. Unacked events stay in a bounded buffer and are
/// resent after reconnect, starting after the cursor the collector reports in `welcome`.
/// When the buffer is full the oldest event is dropped and counted in [`Client::dropped`].
/// Nothing is flushed on drop; call [`Client::flush`] before exit.
pub struct Client {
    path: PathBuf,
    source_id: SourceId,
    capacity: usize,
    /// `(source_seq, serialized event)`, oldest first.
    buffer: VecDeque<(u64, String)>,
    conn: Option<BufReader<UnixStream>>,
    cursor: Option<u64>,
    dropped: u64,
    backoff: Duration,
    retry_at: Instant,
    last_error: Option<String>,
}

impl Client {
    pub fn new(path: impl Into<PathBuf>, source_id: SourceId, capacity: usize) -> Self {
        Self {
            path: path.into(),
            source_id,
            capacity: capacity.max(1),
            buffer: VecDeque::new(),
            conn: None,
            cursor: None,
            dropped: 0,
            backoff: MIN_BACKOFF,
            retry_at: Instant::now(),
            last_error: None,
        }
    }

    /// Buffers `events` and sends what it can without waiting for a reconnect backoff.
    /// Fails only for events this client can never deliver (wrong source, over the line limit).
    pub fn send(&mut self, events: &[CanonicalEvent]) -> anyhow::Result<()> {
        for e in events {
            ensure!(e.source_id == self.source_id, "event {} has source {}, client is {}", e.event_id, e.source_id, self.source_id);
            let line = serde_json::to_string(e)?;
            ensure!(line.len() + FRAME_HEAD.len() + FRAME_TAIL.len() <= MAX_LINE_BYTES, "event {} exceeds {MAX_LINE_BYTES} bytes", e.event_id);
            if self.buffer.len() == self.capacity {
                self.buffer.pop_front();
                self.dropped += 1;
            }
            self.buffer.push_back((e.source_seq, line));
        }
        let _ = self.pump();
        Ok(())
    }

    /// Retries until every buffered event is acked or `timeout` passes. Returns the collector cursor.
    pub fn flush(&mut self, timeout: Duration) -> anyhow::Result<Option<u64>> {
        let deadline = Instant::now() + timeout;
        loop {
            let _ = self.pump();
            if self.buffer.is_empty() {
                return Ok(self.cursor);
            }
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "{} events unacked after {timeout:?} ({} dropped); last error: {}",
                    self.buffer.len(),
                    self.dropped,
                    self.last_error.as_deref().unwrap_or("none")
                );
            }
            std::thread::sleep(self.retry_at.saturating_duration_since(now).min(deadline - now).max(Duration::from_millis(1)));
        }
    }

    /// Events discarded because the buffer overflowed. Delivery is lossy once this is non-zero.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn pending(&self) -> usize {
        self.buffer.len()
    }

    /// Highest contiguous `source_seq` the collector last reported as persisted.
    pub fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    fn pump(&mut self) -> anyhow::Result<()> {
        let r = self.try_pump();
        if let Err(e) = &r {
            self.conn = None;
            self.retry_at = Instant::now() + self.backoff;
            self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
            self.last_error = Some(format!("{e:#}"));
        }
        r
    }

    fn try_pump(&mut self) -> anyhow::Result<()> {
        while !self.buffer.is_empty() {
            let Some(conn) = &mut self.conn else {
                if Instant::now() < self.retry_at {
                    return Ok(());
                }
                self.connect()?;
                continue;
            };
            let mut frame = String::from(FRAME_HEAD);
            let mut n = 0;
            for (_, line) in &self.buffer {
                if n > 0 && frame.len() + 1 + line.len() + FRAME_TAIL.len() > MAX_LINE_BYTES {
                    break;
                }
                if n > 0 {
                    frame.push(',');
                }
                frame.push_str(line);
                n += 1;
            }
            frame.push_str(FRAME_TAIL);
            conn.get_mut().write_all(frame.as_bytes())?;
            let reply = read_reply(conn)?;
            match reply["type"].as_str() {
                Some("ack") => {
                    self.cursor = reply["cursor"].as_u64();
                    self.buffer.drain(..n);
                }
                _ => bail!("collector rejected events: {reply}"),
            }
        }
        Ok(())
    }

    fn connect(&mut self) -> anyhow::Result<()> {
        let stream = UnixStream::connect(&self.path).with_context(|| format!("connect {}", self.path.display()))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let mut conn = BufReader::new(stream);
        let hello = json!({"type": "hello", "protocol": PROTOCOL_VERSION, "source_id": self.source_id});
        conn.get_mut().write_all(format!("{hello}\n").as_bytes())?;
        let reply = read_reply(&mut conn)?;
        ensure!(reply["type"] == "welcome" && reply["protocol"] == PROTOCOL_VERSION, "unexpected handshake reply: {reply}");
        self.cursor = reply["cursor"].as_u64();
        if let Some(c) = self.cursor {
            self.buffer.retain(|(seq, _)| *seq > c);
        }
        self.backoff = MIN_BACKOFF;
        self.conn = Some(conn);
        Ok(())
    }
}

fn read_reply(conn: &mut BufReader<UnixStream>) -> anyhow::Result<Value> {
    let mut line = String::new();
    if conn.take(MAX_LINE_BYTES as u64).read_line(&mut line)? == 0 {
        bail!("collector closed the connection");
    }
    Ok(serde_json::from_str(&line)?)
}
