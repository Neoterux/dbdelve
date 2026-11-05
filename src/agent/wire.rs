//! The frames a call is made of: newline-delimited JSON, one object per line,
//! at most [`FRAME_LIMIT`] bytes. A connection opens with one [`Hello`] and is
//! answered with one `Reply` line.

use std::io::{self, BufRead, Read};

use serde::{Deserialize, Serialize};

use super::tools::Tool;

/// The version the first frame must carry. A frame from another one is
/// refused rather than read as if it meant what this build's frames mean.
pub(super) const PROTOCOL_VERSION: u32 = 1;

// ponytail: a first frame is a tool name and a profile name, so 64 KiB is far
// beyond it; the cap is what stops a connection from making the app buffer
// without end.
pub(super) const FRAME_LIMIT: u64 = 64 * 1024;

/// The first frame of a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Hello {
    Call {
        v: u32,
        tool: Tool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
    },
    /// An agent is attached. Asks nothing, and ends when the connection does.
    Presence { v: u32 },
}

/// One frame as a line ending in `\n`.
pub(super) fn encode<T: Serialize>(frame: &T) -> String {
    let mut line = serde_json::to_string(frame)
        .expect("a frame of strings, numbers and unit enums always serializes");
    line.push('\n');
    line
}

/// The next frame, without its newline. `None` is a reader with nothing left;
/// a line longer than `limit`, one cut short by the end of the stream, or one
/// that is not UTF-8 is an error.
pub(super) fn read_frame(reader: &mut impl BufRead, limit: u64) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = reader.by_ref().take(limit).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        let reason = if read as u64 >= limit {
            "a frame ran past its size limit without ending"
        } else {
            "a frame ended before its newline"
        };
        return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a frame was not UTF-8"))
}

/// Read a first frame, refusing an unknown field, kind or tool and any
/// version but [`PROTOCOL_VERSION`].
pub(super) fn decode_hello(line: &str) -> Result<Hello, String> {
    let hello: Hello = serde_json::from_str(line)
        .map_err(|error| format!("The first frame was refused: {error}."))?;
    let (Hello::Call { v, .. } | Hello::Presence { v }) = &hello;
    if *v != PROTOCOL_VERSION {
        return Err(format!(
            "The first frame speaks version {v}; this build speaks version {PROTOCOL_VERSION}."
        ));
    }
    Ok(hello)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::agent::{Reply, Status};

    #[test]
    fn a_call_frame_and_an_answer_read_back_as_written() {
        let hello = Hello::Call {
            v: PROTOCOL_VERSION,
            tool: Tool::ListConnections,
            connection: None,
        };
        let line = encode(&hello);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let mut reader = Cursor::new(line.into_bytes());
        let frame = read_frame(&mut reader, FRAME_LIMIT)
            .expect("the frame reads")
            .expect("there is a frame");
        assert_eq!(decode_hello(&frame), Ok(hello));

        let reply = Reply {
            status: Status::Executed,
            text: "[]".to_string(),
        };
        let line = encode(&reply);
        assert_eq!(
            serde_json::from_str::<Reply>(line.trim_end()).expect("the answer decodes"),
            reply
        );

        let codes: Vec<String> = [
            Status::Executed,
            Status::Rejected,
            Status::RefusedByGate,
            Status::Cancelled,
            Status::Expired,
            Status::AppNotRunning,
        ]
        .map(|status| serde_json::to_string(&status).expect("a status serializes"))
        .into();
        assert_eq!(
            codes,
            [
                "\"executed\"",
                "\"rejected\"",
                "\"refused_by_gate\"",
                "\"cancelled\"",
                "\"expired\"",
                "\"app_not_running\""
            ]
        );
    }

    #[test]
    fn a_frame_with_an_unknown_field_or_another_version_is_refused() {
        for frame in [
            r#"{"v":1,"kind":"call","tool":"list_connections","extra":1}"#,
            r#"{"v":2,"kind":"call","tool":"list_connections"}"#,
            r#"{"v":1,"kind":"shout","tool":"list_connections"}"#,
            r#"{"v":1,"kind":"call","tool":"drop_everything"}"#,
            "",
            "not json",
        ] {
            assert!(decode_hello(frame).is_err(), "{frame:?} was accepted");
        }
        assert!(decode_hello(r#"{"v":1,"kind":"call","tool":"list_connections"}"#).is_ok());
        assert!(decode_hello(r#"{"v":1,"kind":"presence"}"#).is_ok());
        assert!(decode_hello(r#"{"v":3,"kind":"presence"}"#).is_err());

        let mut long = Cursor::new(vec![b'a'; FRAME_LIMIT as usize + 10]);
        let error = read_frame(&mut long, FRAME_LIMIT).expect_err("a frame with no end is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let mut empty = Cursor::new(Vec::new());
        assert!(matches!(read_frame(&mut empty, FRAME_LIMIT), Ok(None)));
    }
}
