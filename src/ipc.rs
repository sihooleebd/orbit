//! Remote control over a Unix socket: `orbit ctl next`, `orbit ctl vol +5`,
//! `orbit ctl status "{artist} - {title}"`. One request per connection: the client writes one
//! command line (the command-palette language) and reads back one reply (text, ends with '\n').
//! Replies starting with "error: " are failures (the client exits non-zero).
//!
//! Details any client (`socat`, `nc -U`, scripts) can rely on:
//! - The socket file is private to the user (mode 0600).
//! - A request is one line of at most 64 KiB ("\r\n" is fine). Closing the write half instead of
//!   sending '\n' also ends it. A client has 5 s to deliver its line.
//! - Connections that close without sending anything (liveness probes) get no reply.
//! - A socket path too long for the OS (sun_path is ~104 bytes) is transparently replaced by a
//!   short stand-in in the temp dir (see [`socket_path`]), identically on server and client.

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Longest request line accepted, in bytes.
const MAX_LINE: usize = 64 * 1024;
/// How long a client may take to deliver its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the client waits for the reply, and the server for a slow reader to take it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections waiting for their line at once; more are closed right away.
const MAX_PENDING: usize = 64;

pub struct IpcServer {
    listener: UnixListener,
    path: PathBuf,
    /// (device, inode) of our socket file, so `Drop` never deletes a newer orbit's socket.
    id: (u64, u64),
    /// Accepted connections whose request line hasn't fully arrived yet.
    pending: Vec<Pending>,
}

struct Pending {
    stream: UnixStream,
    buf: Vec<u8>,
    since: Instant,
}

/// What reading a pending connection produced.
enum Progress {
    /// Need more bytes.
    Waiting,
    /// `buf` holds the complete line.
    Line,
    TooLong,
    /// Closed without sending anything, or broken.
    Gone,
}

/// A pending request; answer it with `reply` (dropping it without a reply closes the connection).
pub struct IpcRequest {
    pub line: String,
    stream: UnixStream,
}

impl IpcRequest {
    /// Send `text` + "\n" and close. Never blocks the caller: if the client isn't reading fast
    /// enough, the rest is written by a helper thread.
    pub fn reply(self, text: &str) {
        let mut data = Vec::with_capacity(text.len() + 1);
        data.extend_from_slice(text.as_bytes());
        data.push(b'\n');
        send_and_close(self.stream, data);
    }
}

fn send_and_close(mut stream: UnixStream, data: Vec<u8>) {
    let _ = stream.set_nonblocking(true);
    let mut done = 0;
    while done < data.len() {
        match stream.write(&data[done..]) {
            Ok(0) => return,
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                let _ = std::thread::Builder::new().name("ipc-reply".into()).spawn(move || {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_write_timeout(Some(REPLY_TIMEOUT));
                    let _ = stream.write_all(&data[done..]);
                });
                return;
            }
            Err(_) => return,
        }
    }
}

impl IpcServer {
    /// Bind (non-blocking). A stale socket file (nobody listening) is replaced; if another orbit is
    /// running and listening, returns an error (AddrInUse). The socket file gets mode 0600; its
    /// folder is created if needed. A file at `path` that isn't a socket is never touched.
    pub fn bind(path: &Path) -> io::Result<IpcServer> {
        let path = socket_path(path);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() => match UnixStream::connect(&path) {
                Ok(_) => {
                    let msg = format!("another orbit is already running (listening on {})", path.display());
                    return Err(io::Error::new(ErrorKind::AddrInUse, msg));
                }
                // left behind by an orbit that didn't exit cleanly
                Err(e) if e.kind() == ErrorKind::ConnectionRefused => fs::remove_file(&path)?,
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            },
            Ok(_) => {
                let msg = format!("{} exists and is not a socket", path.display());
                return Err(io::Error::new(ErrorKind::AlreadyExists, msg));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let meta = fs::symlink_metadata(&path)?;
        Ok(IpcServer { listener, path, id: (meta.dev(), meta.ino()), pending: Vec::new() })
    }

    /// The listener and the connections still sending their line: the main loop wakes when one
    /// is readable, so requests are served at once rather than on the next tick.
    pub fn fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        std::iter::once(self.listener.as_raw_fd()).chain(self.pending.iter().map(|p| p.stream.as_raw_fd()))
    }

    /// Accept all pending connections and return the requests whose line has arrived. Never blocks:
    /// sockets are non-blocking and a line that is still arriving is picked up by a later call.
    pub fn poll(&mut self) -> Vec<IpcRequest> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // over the limit (or unusable): dropping the stream closes it
                    if self.pending.len() < MAX_PENDING && stream.set_nonblocking(true).is_ok() {
                        self.pending.push(Pending { stream, buf: Vec::new(), since: Instant::now() });
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                // WouldBlock: nothing more to accept; anything else (EMFILE, ...): retry next poll
                Err(_) => break,
            }
        }
        let mut ready = Vec::new();
        for mut p in std::mem::take(&mut self.pending) {
            match read_line(&mut p) {
                Progress::Waiting if p.since.elapsed() < REQUEST_TIMEOUT => self.pending.push(p),
                Progress::Waiting | Progress::Gone => {}
                Progress::TooLong => send_and_close(p.stream, b"error: request too long (max 64 KiB)\n".to_vec()),
                Progress::Line => {
                    let mut line = String::from_utf8_lossy(&p.buf).into_owned();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                    ready.push(IpcRequest { line, stream: p.stream });
                }
            }
        }
        ready
    }
}

/// Read what has arrived on `p` without blocking.
fn read_line(p: &mut Pending) -> Progress {
    let mut chunk = [0u8; 4096];
    loop {
        match p.stream.read(&mut chunk) {
            // EOF: a line without '\n' still counts; nothing at all was a probe
            Ok(0) => return if p.buf.is_empty() { Progress::Gone } else { Progress::Line },
            Ok(n) => {
                let newline = chunk[..n].iter().position(|&b| b == b'\n');
                p.buf.extend_from_slice(&chunk[..newline.unwrap_or(n)]);
                if p.buf.len() > MAX_LINE {
                    return Progress::TooLong;
                }
                if newline.is_some() {
                    return Progress::Line;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Progress::Waiting,
            Err(_) => return Progress::Gone,
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.id) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The socket path really used for `path`: `path` itself when the OS accepts it as a socket
/// address, otherwise (too long, sun_path is ~104 bytes) `$TMPDIR/orbit-<hash of path>.sock`.
/// Server and client both go through this, so they always agree.
pub fn socket_path(path: &Path) -> PathBuf {
    if SocketAddr::from_pathname(path).is_ok() {
        return path.to_path_buf();
    }
    // FNV-1a: unlike DefaultHasher it's stable across Rust versions, so an upgraded client still
    // finds an older running orbit.
    let hash = path.as_os_str().as_bytes().iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    std::env::temp_dir().join(format!("orbit-{hash:016x}.sock"))
}

/// Client: send one command line to a running orbit and return its reply (without the trailing
/// newline). Err with a friendly message if no orbit is running.
pub fn send(path: &Path, line: &str) -> anyhow::Result<String> {
    let sock = socket_path(path);
    let mut stream = UnixStream::connect(&sock).map_err(|e| match e.kind() {
        ErrorKind::NotFound => anyhow::anyhow!("orbit is not running (no socket at {})", sock.display()),
        ErrorKind::ConnectionRefused => anyhow::anyhow!("orbit is not running (stale socket at {})", sock.display()),
        _ => anyhow::anyhow!("can't connect to orbit at {}: {e}", sock.display()),
    })?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    // the protocol is one line per request
    let mut request = line.replace(['\r', '\n'], " ");
    request.push('\n');
    stream.write_all(request.as_bytes()).map_err(|e| anyhow::anyhow!("sending to orbit failed: {e}"))?;
    // tells the server the request is complete; it may already have replied and closed
    let _ = stream.shutdown(Shutdown::Write);
    let mut reply = Vec::new();
    match stream.read_to_end(&mut reply) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::ConnectionReset && !reply.is_empty() => {}
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            anyhow::bail!("orbit didn't reply within {} s", REPLY_TIMEOUT.as_secs())
        }
        Err(e) => anyhow::bail!("reading orbit's reply failed: {e}"),
    }
    let mut reply = String::from_utf8_lossy(&reply).into_owned();
    if reply.ends_with('\n') {
        reply.pop();
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh socket path in the temp dir, removed on drop.
    struct TempSock(PathBuf);

    impl TempSock {
        fn new() -> TempSock {
            static N: AtomicUsize = AtomicUsize::new(0);
            let name = format!("orbit-test-{}-{}.sock", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            TempSock(std::env::temp_dir().join(name))
        }
    }

    impl Drop for TempSock {
        fn drop(&mut self) {
            let _ = fs::remove_file(socket_path(&self.0));
        }
    }

    /// Poll until `n` requests have arrived (or panic after 5 s).
    fn wait_for(server: &mut IpcServer, n: usize) -> Vec<IpcRequest> {
        let start = Instant::now();
        let mut got = Vec::new();
        while got.len() < n {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {n} requests");
            got.extend(server.poll());
            std::thread::sleep(Duration::from_millis(2));
        }
        got
    }

    /// Serve `n` requests on a thread, answering each with `reply(line)` as it arrives.
    fn serve(mut server: IpcServer, n: usize, reply: fn(&str) -> String) -> std::thread::JoinHandle<IpcServer> {
        std::thread::spawn(move || {
            for _ in 0..n {
                let req = wait_for(&mut server, 1).pop().unwrap();
                let text = reply(&req.line);
                req.reply(&text);
            }
            server
        })
    }

    #[test]
    fn round_trip() {
        let sock = TempSock::new();
        let server = IpcServer::bind(&sock.0).unwrap();
        let mode = fs::metadata(&sock.0).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let handle = serve(server, 1, |line| format!("got [{line}]"));
        assert_eq!(send(&sock.0, "vol +5").unwrap(), "got [vol +5]");
        drop(handle.join().unwrap());
        assert!(!sock.0.exists(), "socket file is removed when the server drops");
    }

    #[test]
    fn multiline_error_and_empty_replies() {
        let sock = TempSock::new();
        let handle = serve(IpcServer::bind(&sock.0).unwrap(), 3, |line| match line {
            "status" => "a\nb".into(),
            "bogus" => "error: unknown command".into(),
            _ => String::new(),
        });
        assert_eq!(send(&sock.0, "status").unwrap(), "a\nb");
        assert_eq!(send(&sock.0, "bogus").unwrap(), "error: unknown command");
        assert_eq!(send(&sock.0, "next").unwrap(), "");
        handle.join().unwrap();
    }

    #[test]
    fn newlines_in_a_command_stay_one_line() {
        let sock = TempSock::new();
        let handle = serve(IpcServer::bind(&sock.0).unwrap(), 1, |line| line.to_string());
        assert_eq!(send(&sock.0, "status {title}\n{artist}").unwrap(), "status {title} {artist}");
        handle.join().unwrap();
    }

    #[test]
    fn raw_clients_crlf_eof_slow_and_probes() {
        let sock = TempSock::new();
        let mut server = IpcServer::bind(&sock.0).unwrap();

        // a liveness probe (connect + close) is not a request
        drop(UnixStream::connect(&sock.0).unwrap());
        // CRLF line ending
        let mut crlf = UnixStream::connect(&sock.0).unwrap();
        crlf.write_all(b"next\r\n").unwrap();
        // no newline, ends with EOF instead
        let mut eof = UnixStream::connect(&sock.0).unwrap();
        eof.write_all(b"prev").unwrap();
        eof.shutdown(Shutdown::Write).unwrap();
        // arrives in two parts across polls
        let mut slow = UnixStream::connect(&sock.0).unwrap();
        slow.write_all(b"vol ").unwrap();

        let mut lines: Vec<String> = wait_for(&mut server, 2).into_iter().map(|r| r.line).collect();
        lines.sort();
        assert_eq!(lines, ["next", "prev"]);
        assert!(server.poll().is_empty(), "half a line is not a request yet");
        slow.write_all(b"50\n").unwrap();
        let req = wait_for(&mut server, 1).pop().unwrap();
        assert_eq!(req.line, "vol 50");
        req.reply("ok");
        let mut reply = String::new();
        slow.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "ok\n");
    }

    #[test]
    fn big_reply_never_blocks_the_server() {
        let sock = TempSock::new();
        let mut server = IpcServer::bind(&sock.0).unwrap();
        let mut client = UnixStream::connect(&sock.0).unwrap();
        client.write_all(b"dump\n").unwrap();
        let req = wait_for(&mut server, 1).pop().unwrap();
        let big: String = (0..200_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let start = Instant::now();
        req.reply(&big); // far more than the socket buffer, and nobody is reading yet
        assert!(start.elapsed() < Duration::from_millis(200), "reply blocked for {:?}", start.elapsed());
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = String::new();
        client.read_to_string(&mut got).unwrap();
        assert_eq!(got.len(), big.len() + 1);
        assert!(got.starts_with(&big) && got.ends_with('\n'));
    }

    #[test]
    fn oversized_request_is_rejected() {
        let sock = TempSock::new();
        let mut server = IpcServer::bind(&sock.0).unwrap();
        let mut writer = UnixStream::connect(&sock.0).unwrap();
        let mut reader = writer.try_clone().unwrap();
        let writer = std::thread::spawn(move || {
            // fails with EPIPE once the server gives up on us
            let _ = writer.write_all(&vec![b'x'; MAX_LINE + 10_000]);
        });
        let reader = std::thread::spawn(move || {
            reader.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reply = String::new();
            let _ = reader.read_to_string(&mut reply);
            reply
        });
        let start = Instant::now();
        while !reader.is_finished() {
            assert!(server.poll().is_empty());
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
        let reply = reader.join().unwrap();
        assert!(reply.starts_with("error: request too long"), "{reply}");
        writer.join().unwrap();
    }

    #[test]
    fn stale_socket_is_replaced() {
        let sock = TempSock::new();
        // a listener dropped without unlinking leaves a socket file nobody listens on
        drop(UnixListener::bind(&sock.0).unwrap());
        assert!(fs::symlink_metadata(&sock.0).unwrap().file_type().is_socket());
        assert!(send(&sock.0, "next").unwrap_err().to_string().contains("stale socket"));
        let handle = serve(IpcServer::bind(&sock.0).unwrap(), 1, |_| "alive".into());
        assert_eq!(send(&sock.0, "status").unwrap(), "alive");
        handle.join().unwrap();
    }

    #[test]
    fn second_server_gets_addr_in_use() {
        let sock = TempSock::new();
        let mut first = IpcServer::bind(&sock.0).unwrap();
        let err = IpcServer::bind(&sock.0).err().expect("second bind must fail");
        assert_eq!(err.kind(), ErrorKind::AddrInUse);
        // the probe didn't break the first server, and it still answers
        assert!(first.poll().is_empty());
        let handle = serve(first, 1, |_| "first".into());
        assert_eq!(send(&sock.0, "status").unwrap(), "first");
        handle.join().unwrap();
    }

    #[test]
    fn non_socket_file_is_left_alone() {
        let sock = TempSock::new();
        fs::write(&sock.0, "precious").unwrap();
        let err = IpcServer::bind(&sock.0).err().expect("must refuse");
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&sock.0).unwrap(), "precious");
    }

    #[test]
    fn drop_keeps_a_newer_servers_socket() {
        let sock = TempSock::new();
        let old = IpcServer::bind(&sock.0).unwrap();
        fs::remove_file(&sock.0).unwrap();
        let new = IpcServer::bind(&sock.0).unwrap();
        drop(old);
        assert!(sock.0.exists(), "the old server must not delete the new socket");
        drop(new);
        assert!(!sock.0.exists());
    }

    #[test]
    fn not_running_is_a_friendly_error() {
        let sock = TempSock::new();
        let err = send(&sock.0, "next").unwrap_err().to_string();
        assert!(err.starts_with("orbit is not running (no socket at"), "{err}");
    }

    #[test]
    fn overlong_paths_use_a_short_stand_in() {
        let dir = std::env::temp_dir().join(format!("orbit-test-{}-{}", std::process::id(), "x".repeat(120)));
        let long = dir.join("orbit.sock");
        assert!(SocketAddr::from_pathname(&long).is_err(), "test path must be too long for the OS");
        let short = socket_path(&long);
        assert_ne!(short, long);
        assert_eq!(short, socket_path(&long), "stable");
        assert_ne!(short, socket_path(&dir.join("other.sock")));
        let normal = std::env::temp_dir().join("orbit.sock");
        assert_eq!(socket_path(&normal), normal);

        let _cleanup = TempSock(long.clone());
        let server = IpcServer::bind(&long).unwrap();
        assert!(fs::symlink_metadata(&short).unwrap().file_type().is_socket());
        let handle = serve(server, 1, |line| line.to_uppercase());
        assert_eq!(send(&long, "next").unwrap(), "NEXT");
        handle.join().unwrap();
        let _ = fs::remove_dir(&dir);
    }
}
