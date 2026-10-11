//! `u32` little-endian length + JSON body. Small enough to speak from a
//! shell with `printf` and `xxd`; the length cap keeps a hostile client from
//! making the daemon allocate.

use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte cap")]
    TooLarge(u32),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// `Ok(None)` on a clean EOF before any length byte.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Request, Response};

    #[test]
    fn round_trip_and_eof() {
        let mut buf = Vec::new();
        let req = Request {
            id: 7,
            args: vec!["focus".into(), "left".into()],
            protocol: 1,
        };
        write_frame(&mut buf, &req).unwrap();
        let resp = Response::ok(7, "done");
        write_frame(&mut buf, &resp).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let got: Request = read_frame(&mut cur).unwrap().unwrap();
        assert_eq!(got, req);
        let got: Response = read_frame(&mut cur).unwrap().unwrap();
        assert_eq!(got, resp);
        let eof: Option<Response> = read_frame(&mut cur).unwrap();
        assert!(eof.is_none());
    }

    #[test]
    fn rejects_oversized_length() {
        let mut bytes = (MAX_FRAME + 1).to_le_bytes().to_vec();
        bytes.extend_from_slice(b"{}");
        let mut cur = std::io::Cursor::new(bytes);
        let r: Result<Option<Response>, _> = read_frame(&mut cur);
        assert!(matches!(r, Err(FrameError::TooLarge(_))));
    }
}
