//! Client side of the control socket, used by every `fleet` command that talks to the daemon.
//!
//! The client is as careful about the socket as the daemon is about its clients: it refuses a
//! symlinked path, a path that is not a socket and a directory owned by someone else, because
//! whoever owns the socket receives the commands (and the approvals) sent through it.

use crate::protocol::{
    encode_request, encode_resize, parse_response, read_frame, read_line_bounded, write_frame,
    KIND_DETACH, KIND_EXIT, KIND_INPUT, KIND_NOTICE, KIND_OUTPUT, KIND_RESIZE, MAX_RESPONSE_BYTES,
};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::limits::{MAX_FRAME_PAYLOAD, MAX_MESSAGE_BYTES};
use fleet_domain::value::Value;
use std::fs;
use std::io::{self, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// How long one call may wait for the daemon's answer (merges and cleanups run git).
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

fn daemon_down(socket: &Path) -> FleetError {
    FleetError::new(
        ErrorCode::DaemonDown,
        format!(
            "the fleet daemon is not running (nothing is listening on {}); start it with `fleet daemon`",
            socket.display()
        ),
    )
}

fn my_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// Checks the socket path without following symlinks.
fn check_socket_path(socket: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(socket) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(daemon_down(socket)),
        Err(e) => return Err(FleetError::io("inspecting the control socket", &e)),
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Err(FleetError::forbidden(format!(
            "{} is a symlink; refusing to send commands through it",
            socket.display()
        )));
    }
    if !kind.is_socket() {
        return Err(FleetError::forbidden(format!(
            "{} is not a socket; refusing to use it",
            socket.display()
        )));
    }
    if meta.uid() != my_uid() {
        return Err(FleetError::forbidden(format!(
            "{} belongs to another user",
            socket.display()
        )));
    }
    if let Some(dir) = socket.parent() {
        let dir_meta = fs::symlink_metadata(dir)
            .map_err(|e| FleetError::io("inspecting the socket directory", &e))?;
        if !dir_meta.file_type().is_dir() || dir_meta.uid() != my_uid() {
            return Err(FleetError::forbidden(format!(
                "{} is not a directory owned by you; refusing to use the socket in it",
                dir.display()
            )));
        }
    }
    Ok(())
}

fn closed_by_daemon() -> FleetError {
    FleetError::internal("the daemon closed the connection")
}

fn map_io(e: &io::Error) -> FleetError {
    match e.kind() {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::UnexpectedEof => closed_by_daemon(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => FleetError::new(
            ErrorCode::Timeout,
            "the daemon did not answer in time; it may be busy",
        ),
        io::ErrorKind::InvalidData => {
            FleetError::internal(format!("the daemon sent an unreadable reply ({e})"))
        }
        _ => FleetError::io("talking to the daemon", e),
    }
}

impl Client {
    pub fn connect(socket: &Path) -> Result<Client> {
        check_socket_path(socket)?;
        let stream = UnixStream::connect(socket).map_err(|e| match e.kind() {
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound => daemon_down(socket),
            _ => FleetError::io("connecting to the daemon", &e),
        })?;
        stream
            .set_read_timeout(Some(CALL_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(WRITE_TIMEOUT)))
            .map_err(|e| FleetError::io("configuring the connection", &e))?;
        let reader = stream
            .try_clone()
            .map_err(|e| FleetError::io("configuring the connection", &e))?;
        Ok(Client {
            writer: stream,
            reader: BufReader::new(reader),
        })
    }

    /// Sends one command and returns the reply's data object. An error reply becomes a
    /// [`FleetError`] with the daemon's code, message and subject.
    pub fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
        let mut line = encode_request(cmd, &args);
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(FleetError::limit(format!(
                "the request is larger than {MAX_MESSAGE_BYTES} bytes"
            )));
        }
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(|e| map_io(&e))?;
        match read_line_bounded(&mut self.reader, MAX_RESPONSE_BYTES) {
            Ok(Some(reply)) => parse_response(&reply),
            Ok(None) => Err(closed_by_daemon()),
            Err(e) => Err(map_io(&e)),
        }
    }

    /// Asks the daemon to attach to a task's terminal. On success the connection carries frames.
    pub fn attach(mut self, task: &str, cols: u16, rows: u16) -> Result<AttachStream> {
        self.call(
            "task.attach",
            Value::object([
                ("task", Value::str(task)),
                ("cols", Value::int(cols)),
                ("rows", Value::int(rows)),
            ]),
        )?;
        // The terminal may be silent for hours; the call timeout no longer applies. Bytes the
        // buffered reader already holds (the first output frames) stay in it.
        self.writer
            .set_read_timeout(None)
            .map_err(|e| FleetError::io("configuring the connection", &e))?;
        Ok(AttachStream {
            reader: AttachReader {
                reader: self.reader,
            },
            writer: AttachWriter {
                stream: self.writer,
            },
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AttachEvent {
    Output(String),
    Exit(String),
    Notice(String),
}

pub struct AttachStream {
    reader: AttachReader,
    writer: AttachWriter,
}

impl AttachStream {
    /// Separate halves so one thread can read output while another sends keystrokes.
    pub fn split(self) -> (AttachReader, AttachWriter) {
        (self.reader, self.writer)
    }
}

pub struct AttachReader {
    reader: BufReader<UnixStream>,
}

impl AttachReader {
    /// The next event from the daemon; `None` once the connection is closed. Frames of an
    /// unknown kind are skipped so a newer daemon can add kinds.
    pub fn next_event(&mut self) -> Result<Option<AttachEvent>> {
        loop {
            let Some((kind, payload)) = read_frame(&mut self.reader).map_err(|e| map_io(&e))?
            else {
                return Ok(None);
            };
            // The daemon already sanitized output; lossy decoding only guards the UTF-8 shape.
            let text = || String::from_utf8_lossy(&payload).into_owned();
            match kind {
                KIND_OUTPUT => return Ok(Some(AttachEvent::Output(text()))),
                KIND_EXIT => return Ok(Some(AttachEvent::Exit(text()))),
                KIND_NOTICE => return Ok(Some(AttachEvent::Notice(text()))),
                _ => {}
            }
        }
    }
}

pub struct AttachWriter {
    stream: UnixStream,
}

impl AttachWriter {
    /// Sends keystrokes, in frames small enough for the protocol.
    pub fn input(&mut self, bytes: &[u8]) -> io::Result<()> {
        for chunk in bytes.chunks(MAX_FRAME_PAYLOAD) {
            write_frame(&mut self.stream, KIND_INPUT, chunk)?;
        }
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        write_frame(&mut self.stream, KIND_RESIZE, &encode_resize(cols, rows))
    }

    /// Leaves the session running and closes the connection.
    pub fn detach(&mut self) -> io::Result<()> {
        write_frame(&mut self.stream, KIND_DETACH, &[])?;
        self.stream.shutdown(Shutdown::Write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_support::{obj, Fake, TestServer};
    use crate::testutil::{wait_until, TempDir};
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    fn serve() -> (TestServer, Arc<Fake>, std::sync::mpsc::Sender<Vec<u8>>) {
        let (fake, tx) = Fake::new();
        let ts = TestServer::start(Arc::clone(&fake) as Arc<dyn crate::server::RequestHandler>);
        (ts, fake, tx)
    }

    #[test]
    fn call_round_trip() {
        let (ts, _fake, _tx) = serve();
        let mut client = Client::connect(&ts.path).unwrap();
        let args = obj(&[("task", Value::str("r1/t1")), ("n", Value::int(3))]);
        let data = client.call("echo", args.clone()).unwrap();
        assert_eq!(data.get("schema").and_then(Value::as_str), Some("fleet.echo/1"));
        assert_eq!(data.get("args"), Some(&args));
        // several calls on one connection, and a null args value
        let data = client.call("ping", Value::Null).unwrap();
        assert_eq!(data.get("schema").and_then(Value::as_str), Some("fleet.ping/1"));
    }

    #[test]
    fn error_replies_keep_code_message_and_subject() {
        let (ts, _fake, _tx) = serve();
        let mut client = Client::connect(&ts.path).unwrap();
        let err = client.call("fail", Value::Null).unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(err.message, "not allowed now");
        assert_eq!(err.subject.as_deref(), Some("a1"));
        let err = client.call("nonsense", Value::Null).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        // the connection survived both errors
        assert!(client.call("ping", Value::Null).is_ok());
    }

    #[test]
    fn a_panicking_handler_is_an_internal_error_for_the_client() {
        let (ts, _fake, _tx) = serve();
        let mut client = Client::connect(&ts.path).unwrap();
        let err = client.call("boom", Value::Null).unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(client.call("ping", Value::Null).is_ok());
    }

    #[test]
    fn oversized_requests_are_refused_locally() {
        let (ts, _fake, _tx) = serve();
        let mut client = Client::connect(&ts.path).unwrap();
        let big = obj(&[("text", Value::str("x".repeat(MAX_MESSAGE_BYTES)))]);
        let err = client.call("echo", big).unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
        // nothing was sent, so the connection is still in sync
        assert!(client.call("ping", Value::Null).is_ok());
    }

    #[test]
    fn a_missing_socket_means_the_daemon_is_down() {
        let dir = TempDir::new("cl");
        let err = Client::connect(&dir.join("s")).err().unwrap();
        assert_eq!(err.code, ErrorCode::DaemonDown);
        assert!(err.message.contains("fleet daemon"), "{}", err.message);
    }

    #[test]
    fn a_stale_socket_file_means_the_daemon_is_down() {
        let dir = TempDir::new("cl");
        let path = dir.join("s");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let err = Client::connect(&path).err().unwrap();
        assert_eq!(err.code, ErrorCode::DaemonDown);
        assert!(err.message.contains("fleet daemon"));
    }

    #[test]
    fn a_symlinked_socket_path_is_refused() {
        let (ts, _fake, _tx) = serve();
        let link = ts.path.with_file_name("l");
        std::os::unix::fs::symlink(&ts.path, &link).unwrap();
        let err = Client::connect(&link).err().unwrap();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(err.message.contains("symlink"));
        // the real path still works
        assert!(Client::connect(&ts.path).is_ok());
    }

    #[test]
    fn a_path_that_is_not_a_socket_is_refused() {
        let dir = TempDir::new("cl");
        let file = dir.write("s", "not a socket");
        let err = Client::connect(&file).err().unwrap();
        assert_eq!(err.code, ErrorCode::Forbidden);
        let err = Client::connect(dir.path()).err().unwrap();
        assert_eq!(err.code, ErrorCode::Forbidden);
    }

    #[test]
    fn a_server_that_hangs_up_without_replying_is_reported() {
        let dir = TempDir::new("cl");
        let path = dir.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let acceptor = std::thread::spawn(move || {
            // accept one connection, read the request, close without answering
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let _ = read_line_bounded(&mut reader, MAX_MESSAGE_BYTES);
            }
        });
        let mut client = Client::connect(&path).unwrap();
        let err = client.call("ping", Value::Null).unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains("closed the connection"), "{}", err.message);
        acceptor.join().unwrap();
    }

    #[test]
    fn a_hostile_reply_cannot_crash_the_client() {
        for reply in [
            b"garbage\n".to_vec(),
            b"{\"v\":1,\"ok\":true,\"data\":5}\n".to_vec(),
            vec![0xff, 0xfe, b'\n'],
        ] {
            let dir = TempDir::new("cl");
            let path = dir.join("s");
            let listener = UnixListener::bind(&path).unwrap();
            let acceptor = std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let _ = read_line_bounded(&mut reader, MAX_MESSAGE_BYTES);
                    let _ = stream.write_all(&reply);
                }
            });
            let mut client = Client::connect(&path).unwrap();
            let err = client.call("ping", Value::Null).unwrap_err();
            assert_eq!(err.code, ErrorCode::Internal);
            acceptor.join().unwrap();
        }
    }

    #[test]
    fn attach_round_trip() {
        let (ts, fake, tx) = serve();
        let client = Client::connect(&ts.path).unwrap();
        let stream = client.attach("r1/t1", 120, 40).unwrap();
        let (mut reader, mut writer) = stream.split();

        // output sent right after the reply must not be lost to the reply's read buffer
        tx.send(b"\x1b[32mhello\x1b[0m\r\n".to_vec()).unwrap();
        match reader.next_event().unwrap() {
            Some(AttachEvent::Output(text)) => assert!(text.contains("hello"), "{text:?}"),
            other => panic!("unexpected {other:?}"),
        }

        writer.input(b"echo hi\n").unwrap();
        writer.resize(90, 25).unwrap();
        assert!(wait_until(3000, || fake.has_event("resize:90x25")));
        assert_eq!(*fake.input.lock().unwrap(), b"echo hi\n".to_vec());

        // a large paste is split into frames and arrives intact and in order
        let paste: Vec<u8> = (0..100_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        writer.input(&paste).unwrap();
        assert!(wait_until(5000, || fake.input.lock().unwrap().len() == 8 + paste.len()));
        assert_eq!(fake.input.lock().unwrap().get(8..), Some(paste.as_slice()));

        writer.detach().unwrap();
        assert!(wait_until(3000, || fake.has_event("dropped")));
        // after the detach the stream ends (a pending frame may still arrive first)
        let mut events = 0;
        while reader.next_event().unwrap().is_some() {
            events += 1;
            assert!(events < 10, "the stream must end after a detach");
        }
    }

    #[test]
    fn attach_reports_the_end_of_the_session() {
        let (ts, _fake, tx) = serve();
        let client = Client::connect(&ts.path).unwrap();
        let (mut reader, _writer) = client.attach("t1", 80, 24).unwrap().split();
        tx.send(b"last words".to_vec()).unwrap();
        drop(tx);
        let mut output = String::new();
        let mut exit = None;
        while let Some(event) = reader.next_event().unwrap() {
            match event {
                AttachEvent::Output(t) => output.push_str(&t),
                AttachEvent::Exit(t) => exit = Some(t),
                AttachEvent::Notice(_) => {}
            }
        }
        assert_eq!(output, "last words");
        assert_eq!(exit.as_deref(), Some("session ended"));
    }

    #[test]
    fn attach_failure_is_an_error_not_a_stream() {
        let (ts, _fake, _tx) = serve();
        // first attach consumes the fake's only session; a second one is a conflict
        let first = Client::connect(&ts.path).unwrap().attach("t1", 80, 24);
        assert!(first.is_ok());
        let second = Client::connect(&ts.path).unwrap().attach("t1", 80, 24);
        assert_eq!(second.err().unwrap().code, ErrorCode::Conflict);
    }

    #[test]
    fn unknown_server_frame_kinds_are_skipped() {
        let dir = TempDir::new("cl");
        let path = dir.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let acceptor = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let _ = read_line_bounded(&mut reader, MAX_MESSAGE_BYTES);
            let reply = format!("{}\n", crate::protocol::encode_ok(&obj(&[("schema", Value::str("fleet.attach/1"))])));
            let _ = stream.write_all(reply.as_bytes());
            let _ = write_frame(&mut stream, 77, b"future");
            let _ = write_frame(&mut stream, KIND_NOTICE, b"careful");
            let _ = write_frame(&mut stream, KIND_OUTPUT, &[0xff, b'o', b'k']);
        });
        let (mut reader, _w) = Client::connect(&path)
            .unwrap()
            .attach("t1", 80, 24)
            .unwrap()
            .split();
        assert_eq!(
            reader.next_event().unwrap(),
            Some(AttachEvent::Notice("careful".to_string()))
        );
        match reader.next_event().unwrap() {
            Some(AttachEvent::Output(t)) => assert!(t.ends_with("ok")),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(reader.next_event().unwrap(), None);
        acceptor.join().unwrap();
    }
}
