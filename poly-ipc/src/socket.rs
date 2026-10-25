use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{Event, Reply, Request};

/// Environment variable carrying the IPC socket path.
pub const SOCKET_ENV: &str = "POLY_SOCKET";

/// Errors from the IPC client.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("${SOCKET_ENV} is not set; is poly running?")]
    NoSocket,
    #[error("I/O error talking to {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed reply from compositor")]
    Decode(#[from] serde_json::Error),
    #[error("{0}")]
    Reply(String),
}

/// Socket path convention: `$XDG_RUNTIME_DIR/poly.$WAYLAND_DISPLAY.$PID.sock`.
pub fn socket_path(runtime_dir: &Path, wayland_display: &OsStr, pid: u32) -> PathBuf {
    let mut name = std::ffi::OsString::from("poly.");
    name.push(wayland_display);
    name.push(format!(".{pid}.sock"));
    return runtime_dir.join(name);
}

/// Blocking IPC client connection.
///
/// Each [`request`](Socket::request) writes one line and reads exactly one
/// reply, so the reader is kept across calls: replies that arrive while the
/// client is busy elsewhere are not lost.
pub struct Socket {
    path: PathBuf,
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Socket {
    /// Connect to the socket named by `$POLY_SOCKET`.
    pub fn connect() -> Result<Self, IpcError> {
        let path = std::env::var_os(SOCKET_ENV).ok_or(IpcError::NoSocket)?;
        return Self::connect_to(PathBuf::from(path));
    }

    /// Connect to an explicit socket path.
    pub fn connect_to(path: PathBuf) -> Result<Self, IpcError> {
        let stream = UnixStream::connect(&path).map_err(|source| {
            return IpcError::Io {
                path: path.clone(),
                source,
            };
        })?;
        let reader = stream.try_clone().map_err(|source| {
            return IpcError::Io {
                path: path.clone(),
                source,
            };
        })?;
        return Ok(Self {
            path,
            stream,
            reader: BufReader::new(reader),
        });
    }

    /// Send one request and wait for its reply.
    pub fn send(&mut self, request: &Request) -> Result<Reply, IpcError> {
        self.write_line(request)?;
        let line = self.read_line()?;
        return Ok(serde_json::from_str(&line)?);
    }

    /// Send one request and turn the compositor's error string into [`IpcError`].
    pub fn request(&mut self, request: &Request) -> Result<crate::Response, IpcError> {
        return match self.send(request)? {
            Ok(response) => Ok(response),
            Err(err) => Err(IpcError::Reply(err)),
        };
    }

    /// Read the next [`Event`] of a stream started with
    /// [`Request::EventStream`](crate::Request::EventStream).
    ///
    /// Returns `Ok(None)` when the compositor closed the stream.
    pub fn read_event(&mut self) -> Result<Option<Event>, IpcError> {
        let line = match self.read_line_opt()? {
            Some(line) => line,
            None => return Ok(None),
        };
        if line.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(serde_json::from_str(&line)?));
    }

    /// How long a read may block before failing; `None` blocks forever.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), IpcError> {
        self.reader
            .get_ref()
            .set_read_timeout(timeout)
            .map_err(|source| return self.io_err(source))?;
        return Ok(());
    }

    /// The socket this client talks to.
    pub fn path(&self) -> &Path {
        return &self.path;
    }

    fn write_line(&mut self, request: &Request) -> Result<(), IpcError> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        self.stream
            .write_all(line.as_bytes())
            .and_then(|()| return self.stream.flush())
            .map_err(|source| return self.io_err(source))?;
        return Ok(());
    }

    /// One line, or an error when the compositor went away.
    fn read_line(&mut self) -> Result<String, IpcError> {
        return match self.read_line_opt()? {
            Some(line) => Ok(line),
            None => Err(self.io_err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the compositor closed the connection",
            ))),
        };
    }

    fn read_line_opt(&mut self) -> Result<Option<String>, IpcError> {
        let mut buf = String::new();
        match self.reader.read_line(&mut buf) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(buf)),
            Err(source) => return Err(self.io_err(source)),
        }
    }

    fn io_err(&self, source: std::io::Error) -> IpcError {
        return IpcError::Io {
            path: self.path.clone(),
            source,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_format() {
        let p = socket_path(Path::new("/run/user/1000"), OsStr::new("wayland-1"), 42);
        assert_eq!(p, PathBuf::from("/run/user/1000/poly.wayland-1.42.sock"));
    }
}
