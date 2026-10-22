use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
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
/// How long the collector may take to answer `hello` or a frame before the client reconnects.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one pump waits for a reply before handing control back to the caller.
const POLL: Duration = Duration::from_millis(2);

/// Collector client for one source. `send` buffers and writes without waiting for acks; [`Client::flush`] waits.
/// Unacked events stay in a bounded buffer and are resent after reconnect, starting after the cursor the
/// collector reports in `welcome`. When the buffer is full the oldest event is dropped and counted in
/// [`Client::dropped`]. Nothing is flushed on drop; call [`Client::flush`] before exit.
pub struct Client {
    path: PathBuf,
    source_id: SourceId,
    capacity: usize,
    /// `(source_seq, serialized event)`, oldest first.
    buffer: VecDeque<(u64, String)>,
    conn: Option<Conn>,
    cursor: Option<u64>,
    dropped: u64,
    backoff: Duration,
    retry_at: Instant,
    last_error: Option<String>,
}

struct Conn {
    reader: BufReader<UnixStream>,
    /// Bytes of a reply line read so far.
    partial: Vec<u8>,
    awaiting: Awaiting,
    reply_by: Instant,
}

/// One message is outstanding at a time, per docs/protocol.md.
enum Awaiting {
    Nothing,
    Welcome,
    /// Ack for a frame whose last event has this `source_seq`.
    Ack(u64),
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

    /// Buffers `events` and writes what it can. Never waits for an ack or a reconnect backoff.
    /// Fails, buffering none of `events`, if any of them can never be delivered: invalid
    /// ([`CanonicalEvent::validate`]), from another source, or over the line limit.
    pub fn send(&mut self, events: &[CanonicalEvent]) -> anyhow::Result<()> {
        let mut lines = Vec::with_capacity(events.len());
        for e in events {
            e.validate().map_err(anyhow::Error::msg)?;
            ensure!(e.source_id == self.source_id, "event {} has source {}, client is {}", e.event_id, e.source_id, self.source_id);
            let line = serde_json::to_string(e)?;
            ensure!(line.len() + FRAME_HEAD.len() + FRAME_TAIL.len() <= MAX_LINE_BYTES, "event {} exceeds {MAX_LINE_BYTES} bytes", e.event_id);
            lines.push((e.source_seq, line));
        }
        for l in lines {
            if self.buffer.len() == self.capacity {
                self.buffer.pop_front();
                self.dropped += 1;
            }
            self.buffer.push_back(l);
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
            if self.conn.is_none() {
                std::thread::sleep(self.retry_at.saturating_duration_since(now).min(deadline - now).max(Duration::from_millis(1)));
            }
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

    /// Advances the conversation as far as it can without waiting more than [`POLL`] for a reply.
    fn try_pump(&mut self) -> anyhow::Result<()> {
        loop {
            if self.conn.is_none() {
                if self.buffer.is_empty() || Instant::now() < self.retry_at {
                    return Ok(());
                }
                self.connect()?;
            }
            let conn = self.conn.as_mut().expect("connected above");
            if let Awaiting::Nothing = conn.awaiting {
                let Some((first_seq, first)) = self.buffer.front() else { return Ok(()) };
                let mut frame = format!("{FRAME_HEAD}{first}");
                let mut last_seq = *first_seq;
                for (seq, line) in self.buffer.iter().skip(1) {
                    if frame.len() + 1 + line.len() + FRAME_TAIL.len() > MAX_LINE_BYTES {
                        break;
                    }
                    frame.push(',');
                    frame.push_str(line);
                    last_seq = *seq;
                }
                frame.push_str(FRAME_TAIL);
                conn.send(&frame, Awaiting::Ack(last_seq))?;
            }
            let Some(reply) = conn.poll()? else { return Ok(()) };
            match (std::mem::replace(&mut conn.awaiting, Awaiting::Nothing), reply["type"].as_str()) {
                (Awaiting::Welcome, Some("welcome")) if reply["protocol"] == PROTOCOL_VERSION => {
                    self.cursor = reply["cursor"].as_u64();
                    if let Some(c) = self.cursor {
                        self.buffer.retain(|(seq, _)| *seq > c);
                    }
                    self.backoff = MIN_BACKOFF;
                }
                (Awaiting::Ack(last_seq), Some("ack")) => {
                    self.cursor = reply["cursor"].as_u64();
                    self.buffer.retain(|(seq, _)| *seq > last_seq);
                }
                _ => bail!("unexpected collector reply: {reply}"),
            }
        }
    }

    fn connect(&mut self) -> anyhow::Result<()> {
        let stream = UnixStream::connect(&self.path).with_context(|| format!("connect {}", self.path.display()))?;
        stream.set_read_timeout(Some(POLL))?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let mut conn = Conn { reader: BufReader::new(stream), partial: Vec::new(), awaiting: Awaiting::Nothing, reply_by: Instant::now() };
        let hello = json!({"type": "hello", "protocol": PROTOCOL_VERSION, "source_id": self.source_id});
        conn.send(&format!("{hello}\n"), Awaiting::Welcome)?;
        self.conn = Some(conn);
        Ok(())
    }
}

impl Conn {
    fn send(&mut self, msg: &str, awaiting: Awaiting) -> anyhow::Result<()> {
        // ponytail: a frame larger than the socket buffer blocks for up to WRITE_TIMEOUT if the collector stops reading.
        self.reader.get_mut().write_all(msg.as_bytes())?;
        self.awaiting = awaiting;
        self.reply_by = Instant::now() + REPLY_TIMEOUT;
        Ok(())
    }

    /// The next reply line, or `None` if none arrived within [`POLL`].
    fn poll(&mut self) -> anyhow::Result<Option<Value>> {
        let limit = (MAX_LINE_BYTES - self.partial.len()) as u64;
        match (&mut self.reader).take(limit).read_until(b'\n', &mut self.partial) {
            Ok(0) => bail!("collector closed the connection"),
            Ok(_) if self.partial.ends_with(b"\n") => {
                let line = std::mem::take(&mut self.partial);
                Ok(Some(serde_json::from_slice(&line)?))
            }
            Ok(_) if self.partial.len() >= MAX_LINE_BYTES => bail!("collector reply exceeds {MAX_LINE_BYTES} bytes"),
            Ok(_) => bail!("collector closed the connection mid-line"),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                ensure!(Instant::now() < self.reply_by, "no reply from collector within {REPLY_TIMEOUT:?}");
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}
