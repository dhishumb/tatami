//! The Unix socket server. One listener thread, one thread per connection,
//! std only. A connection thread never touches window-manager state: it
//! parses a frame, hands the request to the main thread through the channel
//! and blocks until the reply comes back. Subscriptions hand over a sender
//! instead and the main thread pushes events into it.
//!
//! The socket grants control of every window, so the directory is 0700, the
//! socket 0600, and the peer's uid must match ours.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::thread;

use tatami_protocol::{IpcEvent, PROTOCOL_VERSION, Request, Response, read_frame, write_frame};

/// What a connection thread asks the main thread to do.
pub enum IpcMsg {
    Request {
        req: Request,
        reply: SyncSender<Response>,
    },
    /// `subscribe <topics>`: the main thread keeps `sink` and sends matching
    /// events until it fails (reader gone).
    Subscribe {
        topics: Vec<String>,
        ack: SyncSender<Response>,
        sink: Sender<IpcEvent>,
    },
}

/// Pokes the main run loop so it drains the channel; the platform layer
/// supplies the implementation (CFRunLoop wake-up).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct IpcServer {
    path: PathBuf,
}

impl IpcServer {
    /// Bind and start accepting. Fails if another daemon holds the socket.
    pub fn start(path: PathBuf, tx: Sender<crate::MainMsg>, wake: Waker) -> io::Result<IpcServer> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        if path.exists() {
            // Stale socket from a crashed daemon, or a live one. Try to
            // connect: a refusal means stale.
            match UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another tatamid is listening on {}", path.display()),
                    ));
                }
                Err(_) => fs::remove_file(&path)?,
            }
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let my_uid = unsafe_geteuid();
        thread::Builder::new()
            .name("tatami-ipc".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    match conn {
                        Ok(stream) => {
                            if peer_uid(&stream) != Some(my_uid) {
                                tracing::warn!("rejecting socket client with a different uid");
                                continue;
                            }
                            let tx = tx.clone();
                            let wake = wake.clone();
                            let _ = thread::Builder::new()
                                .name("tatami-conn".into())
                                .spawn(move || serve(stream, tx, wake));
                        }
                        Err(e) => {
                            tracing::warn!("accept failed: {e}");
                            thread::sleep(std::time::Duration::from_millis(50));
                        }
                    }
                }
            })?;
        Ok(IpcServer { path })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn serve(mut stream: UnixStream, tx: Sender<crate::MainMsg>, wake: Waker) {
    loop {
        let req: Request = match read_frame(&mut stream) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                tracing::debug!("client frame error: {e}");
                return;
            }
        };
        if req.protocol != 0 && req.protocol != PROTOCOL_VERSION {
            let r = Response::err(
                req.id,
                format!(
                    "protocol mismatch: client {} vs daemon {PROTOCOL_VERSION}; reinstall both binaries",
                    req.protocol
                ),
            );
            let _ = write_frame(&mut stream, &r);
            return;
        }
        if req.args.first().map(String::as_str) == Some("subscribe") {
            let (ack_tx, ack_rx) = sync_channel(1);
            let (sink_tx, sink_rx) = std::sync::mpsc::channel();
            let topics = req.args[1..].to_vec();
            if tx
                .send(
                    IpcMsg::Subscribe {
                        topics,
                        ack: ack_tx,
                        sink: sink_tx,
                    }
                    .into(),
                )
                .is_err()
            {
                return;
            }
            wake();
            let Ok(mut ack) = ack_rx.recv() else { return };
            ack.id = req.id;
            let ok = ack.ok;
            if write_frame(&mut stream, &ack).is_err() || !ok {
                return;
            }
            pump_events(stream, sink_rx);
            return;
        }
        let (reply_tx, reply_rx) = sync_channel(1);
        let id = req.id;
        if tx
            .send(
                IpcMsg::Request {
                    req,
                    reply: reply_tx,
                }
                .into(),
            )
            .is_err()
        {
            return;
        }
        wake();
        let Ok(mut resp) = reply_rx.recv() else {
            return;
        };
        resp.id = id;
        if write_frame(&mut stream, &resp).is_err() {
            return;
        }
    }
}

/// Forward events until the reader hangs up. Dropping `rx` is how the main
/// thread learns the subscriber is gone: its next `send` fails and it
/// removes the sink.
fn pump_events(mut stream: UnixStream, rx: Receiver<IpcEvent>) {
    for ev in rx {
        if write_frame(&mut stream, &ev).is_err() {
            return;
        }
    }
}

fn unsafe_geteuid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc_geteuid() }
}

unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
    #[link_name = "getpeereid"]
    fn libc_getpeereid(fd: i32, euid: *mut u32, egid: *mut u32) -> i32;
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut uid: u32 = 0;
    let mut gid: u32 = 0;
    // SAFETY: fd is a live socket owned by `stream`; both out-pointers are valid.
    let rc = unsafe { libc_getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn tmp_sock(tag: &str) -> PathBuf {
        // macOS caps socket paths at 104 bytes; keep it short and unique.
        let dir = std::env::temp_dir().join(format!("tt{}{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("sock")
    }

    #[test]
    fn request_round_trip_and_subscribe() {
        let path = tmp_sock("a");
        let (tx, rx) = channel();
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w2 = woke.clone();
        let server = IpcServer::start(
            path.clone(),
            tx,
            Arc::new(move || {
                w2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        )
        .unwrap();

        // Fake main thread.
        let main = thread::spawn(move || {
            let mut sinks = Vec::new();
            for msg in rx {
                let crate::MainMsg::Ipc(msg) = msg else {
                    continue;
                };
                match msg {
                    IpcMsg::Request { req, reply } => {
                        let text = req.args.join(" ");
                        reply.send(Response::ok(0, format!("echo {text}"))).unwrap();
                        if text == "stop" {
                            break;
                        }
                    }
                    IpcMsg::Subscribe { topics, ack, sink } => {
                        ack.send(Response::ok(0, "")).unwrap();
                        sink.send(IpcEvent::ModeChanged {
                            mode: Some(topics.join(",")),
                        })
                        .unwrap();
                        sinks.push(sink);
                    }
                }
            }
        });

        let mut c = UnixStream::connect(&path).unwrap();
        write_frame(
            &mut c,
            &Request {
                id: 9,
                args: vec!["ping".into()],
                protocol: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let r: Response = read_frame(&mut c).unwrap().unwrap();
        assert_eq!(r.id, 9);
        assert_eq!(r.stdout, "echo ping");

        let mut s = UnixStream::connect(&path).unwrap();
        write_frame(
            &mut s,
            &Request {
                id: 1,
                args: vec!["subscribe".into(), "mode".into()],
                protocol: 0,
            },
        )
        .unwrap();
        let ack: Response = read_frame(&mut s).unwrap().unwrap();
        assert!(ack.ok);
        let ev: IpcEvent = read_frame(&mut s).unwrap().unwrap();
        assert_eq!(
            ev,
            IpcEvent::ModeChanged {
                mode: Some("mode".into())
            }
        );

        write_frame(
            &mut c,
            &Request {
                id: 2,
                args: vec!["stop".into()],
                protocol: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let _: Response = read_frame(&mut c).unwrap().unwrap();
        main.join().unwrap();
        assert!(woke.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        drop(server);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn protocol_mismatch_is_refused() {
        let path = tmp_sock("b");
        let (tx, _rx) = channel();
        let _server = IpcServer::start(path.clone(), tx, Arc::new(|| {})).unwrap();
        let mut c = UnixStream::connect(&path).unwrap();
        write_frame(
            &mut c,
            &Request {
                id: 1,
                args: vec!["ping".into()],
                protocol: 999,
            },
        )
        .unwrap();
        let r: Response = read_frame(&mut c).unwrap().unwrap();
        assert!(!r.ok);
        assert!(r.error.contains("protocol mismatch"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
