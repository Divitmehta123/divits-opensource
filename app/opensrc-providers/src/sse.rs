use async_stream::try_stream;
use futures::{Stream, StreamExt};
use opensrc_core::ProviderError;
use serde_json::Value;
use std::pin::Pin;

pub enum SseFrame {
    Json(Value),
    Done,
}

// Nested conditions avoid a let-chain expansion incompatibility in async-stream.
#[allow(clippy::collapsible_if)]
pub fn json_frames(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<SseFrame, ProviderError>> + Send>> {
    Box::pin(try_stream! {
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| ProviderError::Transient(error.to_string()))?;
            buffer.extend_from_slice(&chunk);
            while let Some((offset, delimiter_length)) = frame_boundary(&buffer) {
                let frame = buffer.drain(..offset).collect::<Vec<_>>();
                buffer.drain(..delimiter_length);
                if let Some(parsed) = parse_frame(&frame)? {
                    yield parsed;
                }
            }
        }
        if !buffer.is_empty() {
            if let Some(parsed) = parse_frame(&buffer)? {
                yield parsed;
            }
        }
    })
}

fn frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|offset| (offset, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|offset| (offset, 2))
        })
}

fn parse_frame(frame: &[u8]) -> Result<Option<SseFrame>, ProviderError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    if data == "[DONE]" {
        return Ok(Some(SseFrame::Done));
    }
    serde_json::from_str(&data)
        .map(SseFrame::Json)
        .map(Some)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{SseFrame, frame_boundary, parse_frame};

    #[test]
    fn parses_crlf_json_and_done_frames() {
        assert_eq!(frame_boundary(b"data: {}\r\n\r\nnext"), Some((8, 4)));
        let frame = parse_frame(b"data: {\"ok\":true}\r\n").expect("frame");
        assert!(matches!(frame, Some(SseFrame::Json(_))));
        assert!(matches!(
            parse_frame(b"data: [DONE]").expect("done"),
            Some(SseFrame::Done)
        ));
    }
}
