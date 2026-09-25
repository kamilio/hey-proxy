use anyhow::Result;
use serde_json::Value;

/// Bounded SSE framing across arbitrary byte and UTF-8 boundaries.
pub(super) const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    frame_bytes: usize,
    after_cr: bool,
    done: bool,
}
impl SseDecoder {
    fn dispatch(&mut self, events: &mut Vec<Value>) -> Result<()> {
        self.frame_bytes = 0;
        if self.data.is_empty() {
            return Ok(());
        }
        self.data.pop(); // Final data-line newline.
        if self.data.is_empty() {
            return Ok(());
        }
        if self.data == b"[DONE]" {
            self.done = true;
        } else {
            anyhow::ensure!(!self.done, "SSE data arrived after [DONE]");
            events.push(serde_json::from_slice(&self.data)?);
        }
        self.data.clear();
        Ok(())
    }
    fn end_line(&mut self, events: &mut Vec<Value>) -> Result<()> {
        if self.line.is_empty() {
            self.dispatch(events)?;
        } else {
            // Validate ignored fields too; never repair malformed UTF-8.
            std::str::from_utf8(&self.line)?;
            if let Some(data) = self.line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                self.data.extend_from_slice(data);
                self.data.push(b'\n');
            }
            self.line.clear();
        }
        Ok(())
    }
    pub(super) fn feed(&mut self, bytes: &[u8], eof: bool) -> Result<Vec<Value>> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            self.frame_bytes += 1;
            anyhow::ensure!(self.frame_bytes <= MAX_FRAME, "SSE frame exceeds 16 MiB");
            if matches!(byte, b'\n' | b'\r') {
                self.end_line(&mut events)?;
            } else {
                self.line.push(byte);
            }
        }
        if eof {
            if !self.line.is_empty() {
                self.end_line(&mut events)?;
            }
            self.dispatch(&mut events)?;
        }
        Ok(events)
    }
}
