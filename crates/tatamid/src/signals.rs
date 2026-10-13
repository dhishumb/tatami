//! SIGTERM/SIGINT/SIGHUP/SIGUSR1 → a message on the main channel. The
//! handler thread does nothing else; the main thread saves state, un-parks
//! every window and exits (or, for SIGUSR1, hot-swaps to the `tatamid` on
//! disk). SIGKILL skips all of that, which is why startup also scans for
//! parked strays.

use std::sync::mpsc::Sender;
use std::thread;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
use signal_hook::iterator::Signals;

use crate::MainMsg;
use crate::ipc::Waker;

pub fn start(tx: Sender<MainMsg>, wake: Waker) -> std::io::Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP, SIGUSR1])?;
    thread::Builder::new()
        .name("tatami-signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                let msg = match sig {
                    SIGHUP => MainMsg::ReloadConfig,
                    SIGUSR1 => MainMsg::HotSwap("SIGUSR1".into()),
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
