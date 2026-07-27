//! Minimal incremental Server-Sent-Events assembler.
//!
//! Network chunks arrive at arbitrary boundaries; this buffers until a
//! complete `\n\n`-terminated event block is available and yields
//! `(event, data)` pairs. Multiple `data:` lines are joined with `\n`
//! per the SSE spec.

#[derive(Debug, Clone, PartialEq)]
pub struct SseMessage {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct SseAssembler {
    buf: String,
}

impl SseAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a raw chunk; returns all complete messages it finished.
    pub fn push(&mut self, chunk: &str) -> Vec<SseMessage> {
        self.buf.push_str(chunk);
        // Normalize CRLF once over the whole buffer (idempotent).
        if self.buf.contains('\r') {
            self.buf = self.buf.replace("\r\n", "\n");
        }
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let block: String = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            let mut event: Option<String> = None;
            let mut data_lines: Vec<String> = Vec::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                }
                // comments (`:`) and `id:`/`retry:` lines are ignored
            }
            if !data_lines.is_empty() {
                out.push(SseMessage {
                    event,
                    data: data_lines.join("\n"),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_across_arbitrary_chunk_boundaries() {
        let mut a = SseAssembler::new();
        assert!(a.push("data: {\"he").is_empty());
        assert!(a.push("llo\":1}").is_empty());
        let msgs = a.push("\n\ndata: [DONE]\n\n");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].data, "{\"hello\":1}");
        assert_eq!(msgs[1].data, "[DONE]");
    }

    #[test]
    fn carries_event_names_and_crlf() {
        let mut a = SseAssembler::new();
        let msgs = a.push("event: message_start\r\ndata: {\"a\":1}\r\n\r\n");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].event.as_deref(), Some("message_start"));
        assert_eq!(msgs[0].data, "{\"a\":1}");
    }

    #[test]
    fn joins_multiline_data() {
        let mut a = SseAssembler::new();
        let msgs = a.push("data: line1\ndata: line2\n\n");
        assert_eq!(msgs[0].data, "line1\nline2");
    }
}
