//! A process-lifetime parent thread for Linux shell launchers.
//!
//! PR_SET_PDEATHSIG (Bubblewrap --die-with-parent) watches the thread that
//! forked, not its process. Tokio workers can retire after block_in_place;
//! spawning there would kill a healthy sandbox when the worker goes idle.
//!
//! Requires a current Tokio runtime with I/O and signal drivers; its handle
//! registers the child there, while the reply transfers ownership to the caller.
//! Parent-thread failure stays terminal until process restart (not runtime
//! restart): existing sandboxes may already be dead, and a lost reply cannot
//! prove whether a command ran. Never automatically retry that spawn.
use std::{
    io,
    sync::{OnceLock, mpsc},
};

use tokio::{
    process::{Child, Command},
    runtime::Handle,
};

struct Request {
    command: Command,
    runtime: Handle,
    reply: mpsc::SyncSender<io::Result<Child>>,
}

pub(super) fn spawn(command: Command) -> io::Result<Child> {
    // No runtime/session owns this thread. Process death still triggers the
    // kernel fail-safe; ordinary job teardown remains the Guard's responsibility.
    static SPAWNER: OnceLock<mpsc::SyncSender<Request>> = OnceLock::new();
    let runtime = Handle::try_current().map_err(io::Error::other)?;
    let spawner = match SPAWNER.get() {
        Some(spawner) => spawner,
        None => {
            let (send, receive) = mpsc::sync_channel::<Request>(0);
            std::thread::Builder::new()
                .name("dotlink-spawn".into())
                .spawn(move || {
                    for mut request in receive {
                        let _runtime = request.runtime.enter();
                        let _ = request.reply.send(request.command.spawn());
                    }
                })?;
            // Don't cache a transient thread-creation failure. Concurrent losers
            // have no requests; dropping their unused sender ends the empty thread.
            let _ = SPAWNER.set(send);
            SPAWNER.get().expect("spawn sender initialized")
        }
    };
    let (reply, receive) = mpsc::sync_channel(1);
    // Synchronous handoff: cancellation cannot strand a queued spawn or detach
    // its Child before the caller can attach the process-tree guard.
    crate::blocking::run(|| {
        spawner
            .send(Request {
                command,
                runtime,
                reply,
            })
            .map_err(|_| {
                io::Error::other("shell spawn thread stopped; restart the dotlink process")
            })?;
        receive.recv().map_err(|_| {
            io::Error::other("shell spawn thread lost child; restart the dotlink process")
        })?
    })
}
