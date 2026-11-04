use super::protocol::{ProtocolEvent, ProtocolRequest};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::sync::{mpsc, Mutex};

pub trait Transport: Send + Sync + 'static {
    fn read_request(&self) -> io::Result<Option<ProtocolRequest>>;
    fn write_event(&self, event: &ProtocolEvent) -> io::Result<()>;
}

pub struct MemoryTransport {
    request_rx: Mutex<mpsc::Receiver<ProtocolRequest>>,
    event_tx: mpsc::Sender<ProtocolEvent>,
}

impl MemoryTransport {
    pub fn new(
        request_rx: mpsc::Receiver<ProtocolRequest>,
        event_tx: mpsc::Sender<ProtocolEvent>,
    ) -> Self {
        Self {
            request_rx: Mutex::new(request_rx),
            event_tx,
        }
    }
}

impl Transport for MemoryTransport {
    fn read_request(&self) -> io::Result<Option<ProtocolRequest>> {
        match self.request_rx.lock().unwrap().recv() {
            Ok(request) => Ok(Some(request)),
            Err(_) => Ok(None),
        }
    }

    fn write_event(&self, event: &ProtocolEvent) -> io::Result<()> {
        self.event_tx
            .send(event.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "event receiver closed"))
    }
}

// ---------------------------------------------------------------------
// Unix-socket transports (Fase 3a, daemon mode). `#[cfg(unix)]`: framing
// identik StdioTransport (satu JSON per baris + `\n`, baris kosong
// di-skip), hanya medianya yang berbeda.
// ---------------------------------------------------------------------

#[cfg(unix)]
pub struct UnixSocketTransport {
    input: Mutex<BufReader<std::os::unix::net::UnixStream>>,
    output: Mutex<BufWriter<std::os::unix::net::UnixStream>>,
}

#[cfg(unix)]
impl UnixSocketTransport {
    /// Connect to a daemon listening on `path` (client side).
    pub fn connect(path: &std::path::Path) -> io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        Self::from_stream(stream)
    }

    /// Wrap an already-connected stream. The reader gets a `try_clone`
    /// handle; the original stream stays the writer.
    pub fn from_stream(stream: std::os::unix::net::UnixStream) -> io::Result<Self> {
        let reader = stream.try_clone()?;
        Ok(Self {
            input: Mutex::new(BufReader::new(reader)),
            output: Mutex::new(BufWriter::new(stream)),
        })
    }
}

#[cfg(unix)]
impl Transport for UnixSocketTransport {
    fn read_request(&self) -> io::Result<Option<ProtocolRequest>> {
        loop {
            let mut line = String::new();
            let bytes = self.input.lock().unwrap().read_line(&mut line)?;
            if bytes == 0 {
                return Ok(None);
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(line.trim())
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }

    fn write_event(&self, event: &ProtocolEvent) -> io::Result<()> {
        let mut output = self.output.lock().unwrap();
        serde_json::to_writer(&mut *output, event)?;
        output.write_all(b"\n")?;
        output.flush()
    }
}

/// Server-side transport untuk satu koneksi daemon: wraps the stream
/// returned by `UnixListener::accept` (dipakai `RuntimeEngine` per
/// koneksi).
#[cfg(unix)]
pub struct SocketConnectionTransport {
    input: Mutex<BufReader<std::os::unix::net::UnixStream>>,
    output: Mutex<BufWriter<std::os::unix::net::UnixStream>>,
}

#[cfg(unix)]
impl SocketConnectionTransport {
    /// Wrap an accepted stream. The reader gets a `try_clone` handle;
    /// the original stream stays the writer.
    pub fn new(stream: std::os::unix::net::UnixStream) -> io::Result<Self> {
        let reader = stream.try_clone()?;
        Ok(Self {
            input: Mutex::new(BufReader::new(reader)),
            output: Mutex::new(BufWriter::new(stream)),
        })
    }
}

#[cfg(unix)]
impl Transport for SocketConnectionTransport {
    fn read_request(&self) -> io::Result<Option<ProtocolRequest>> {
        loop {
            let mut line = String::new();
            let bytes = self.input.lock().unwrap().read_line(&mut line)?;
            if bytes == 0 {
                return Ok(None);
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(line.trim())
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }

    fn write_event(&self, event: &ProtocolEvent) -> io::Result<()> {
        let mut output = self.output.lock().unwrap();
        serde_json::to_writer(&mut *output, event)?;
        output.write_all(b"\n")?;
        output.flush()
    }
}

#[cfg(unix)]
#[cfg(test)]
mod unix_socket_tests {
    use super::super::protocol::ProtocolEvent;
    use super::*;
    use crate::events::Event;
    use crate::runtime::protocol::Request;
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    /// Unique short socket path (unix path limit ~108 chars).
    fn socket_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("a-sock-{}-{}-{}", std::process::id(), nanos, tag))
    }

    /// Full-duplex round trip over a real unix socket: server
    /// (SocketConnectionTransport) writes Ready, reads the client's
    /// request back, and answers it — the client (UnixSocketTransport)
    /// reads the event frames and writes the request. Both transports
    /// must frame line-delimited JSON in both directions.
    #[test]
    fn socket_transport_round_trips_frames() {
        let path = socket_path("roundtrip");
        let listener = UnixListener::bind(&path).unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let transport = SocketConnectionTransport::new(stream).unwrap();
            // 1) server → client: Ready announcement.
            transport
                .write_event(&ProtocolEvent::new(
                    None,
                    Event::Ready {
                        protocol: "agentic".into(),
                        version: 1,
                    },
                ))
                .unwrap();
            // 2) server ← client: read the client's request back.
            let request = transport
                .read_request()
                .unwrap()
                .expect("client request must arrive");
            assert!(matches!(request.request, Request::Shutdown));
            // 3) server → client: reply tagged with the request id.
            transport
                .write_event(&ProtocolEvent::new(
                    Some(request.id),
                    Event::Done {
                        result: "ack".into(),
                    },
                ))
                .unwrap();
        });

        let client = UnixSocketTransport::connect(&path).unwrap();

        // 1) client ← server: Ready frame arrives as one JSON line.
        let mut line = String::new();
        let bytes = client.input.lock().unwrap().read_line(&mut line).unwrap();
        assert!(bytes > 0, "server event must arrive");
        let ready: ProtocolEvent = serde_json::from_str(line.trim()).unwrap();
        assert!(matches!(ready.event, Event::Ready { .. }));

        // 2) client → server: write a request frame (one JSON line + \n).
        {
            let mut writer = client.output.lock().unwrap();
            serde_json::to_writer(
                &mut *writer,
                &ProtocolRequest::new("ping-1", Request::Shutdown),
            )
            .unwrap();
            writer.write_all(b"\n").unwrap();
            writer.flush().unwrap();
        }

        // 3) client ← server: reply event tagged with the request id.
        let mut line = String::new();
        let bytes = client.input.lock().unwrap().read_line(&mut line).unwrap();
        assert!(bytes > 0, "server reply must arrive");
        let reply: ProtocolEvent = serde_json::from_str(line.trim()).unwrap();
        match reply.event {
            Event::Done { result } => assert_eq!(result, "ack"),
            other => panic!("expected Done reply, got {other:?}"),
        }
        assert_eq!(reply.request_id.as_deref(), Some("ping-1"));

        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// connect() pada path yang tidak ada harus gagal bersih (io::Error),
    /// bukan panic.
    #[test]
    fn connect_to_missing_socket_is_a_clean_error() {
        let path = socket_path("missing");
        assert!(UnixSocketTransport::connect(&path).is_err());
    }
}
