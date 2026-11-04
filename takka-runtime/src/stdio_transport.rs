use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::sync::Mutex;
use takka_core::runtime::protocol::{ProtocolEvent, ProtocolRequest};
use takka_core::runtime::transport::Transport;

pub struct StdioTransport {
    input: Mutex<BufReader<io::Stdin>>,
    output: Mutex<BufWriter<io::Stdout>>,
}

impl StdioTransport {
    pub fn new() -> Self {
        Self {
            input: Mutex::new(BufReader::new(io::stdin())),
            output: Mutex::new(BufWriter::new(io::stdout())),
        }
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for StdioTransport {
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
        serde_json::to_writer(&mut *output, event).map_err(io::Error::other)?;
        output.write_all(b"\n")?;
        output.flush()
    }
}
