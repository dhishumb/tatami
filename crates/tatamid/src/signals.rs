//! SIGTERM/SIGINT/SIGHUP → a message on the main channel. The handler
//! thread does nothing else; the main thread saves state, un-parks every
//! window and exits. SIGKILL skips all of that, which is why startup also
//! scans for parked strays.

use std::sync::mpsc::Sender;
use std::thread;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::MainMsg;
use crate::ipc::Waker;

pub fn start(tx: Sender<MainMsg>, wake: Waker) -> std::io::Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    thread::Builder::new()
        .name("tatami-signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                let msg = match sig {
                    SIGHUP => MainMsg::ReloadConfig,
                    _ => MainMsg::Shutdown(format!("signal {sig}")),
                };
                if tx.send(msg).is_err() {
                    return;
                }
                wake();
            }
        })?;
    Ok(())
}
