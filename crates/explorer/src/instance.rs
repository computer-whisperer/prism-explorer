//! Single-instance arbitration over D-Bus.
//!
//! Only one prism-explorer process per session is the *primary*: it
//! owns the well-known name [`BUS_NAME`], serves the portal FileChooser
//! and FileManager1 services, and owns the shared history/settings
//! stores. A second launch ([`Arbitration::Secondary`]) hands its
//! directory to the primary via [`hand_off`] and exits, so the two
//! processes never race those single-owner stores (each loads a
//! snapshot at startup and writes the whole thing back — concurrent
//! owners would clobber each other).
//!
//! The primary exposes one method, `OpenWindow(ay)`, taking a raw path
//! (paths aren't UTF-8); it posts [`HostCommand::OpenBrowser`] so the
//! resident loop opens a fresh browser window there.

use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use winit::event_loop::EventLoopProxy;

use crate::host::HostCommand;

/// The single-instance well-known name. `io.github.<owner>.<App>` is the
/// freedesktop convention for an app hosted on GitHub without its own
/// domain.
pub const BUS_NAME: &str = "io.github.computer_whisperer.PrismExplorer";
const OBJECT_PATH: &str = "/io/github/computer_whisperer/PrismExplorer";

struct Instance {
    proxy: EventLoopProxy<HostCommand>,
}

#[zbus::interface(name = "io.github.computer_whisperer.PrismExplorer")]
impl Instance {
    /// Open a new browser window at `path` (raw bytes). Invoked by a
    /// second launch handing off to this primary.
    fn open_window(&self, path: Vec<u8>) {
        let dir = PathBuf::from(std::ffi::OsString::from_vec(path));
        tracing::info!(dir = %dir.display(), "instance: open_window");
        let _ = self
            .proxy
            .send_event(HostCommand::OpenBrowser { dir, select: None });
    }
}

/// How this launch relates to the session's single-instance contract.
/// The distinction between `Secondary` and `BusUnavailable` matters:
/// a secondary has a primary to hand off to, while a broken bus means
/// there is nobody to coordinate with — falling back to a lone window
/// is the only way to open at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arbitration {
    /// We own [`BUS_NAME`] and serve `OpenWindow` — this process is the
    /// primary for the session.
    Primary,
    /// Another prism owns the name; [`hand_off`] and exit.
    Secondary,
    /// No usable session bus. The caller should run as a lone,
    /// uncoordinated browser (and must not claim shared D-Bus names).
    BusUnavailable,
}

/// Try to become the primary instance. On [`Arbitration::Primary`] the
/// name is held by a detached thread for the process's lifetime and the
/// `OpenWindow` method is served.
pub fn try_become_primary(proxy: EventLoopProxy<HostCommand>) -> Arbitration {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("instance".into())
        .spawn(move || match serve(proxy) {
            Ok(Some(_conn)) => {
                tracing::info!("primary instance: serving {BUS_NAME}");
                let _ = ready_tx.send(Arbitration::Primary);
                // The connection's executor handles calls; park to keep
                // it (and the owned name) alive for the process lifetime.
                loop {
                    std::thread::park();
                }
            }
            Ok(None) => {
                // Another prism already owns the name — we are secondary.
                tracing::info!("another prism owns {BUS_NAME}; this launch is secondary");
                let _ = ready_tx.send(Arbitration::Secondary);
            }
            Err(e) => {
                // No session bus, etc. Run as a lone instance rather than
                // aborting, but we are not a coordinating primary.
                tracing::info!(error = %e, "instance bus unavailable");
                let _ = ready_tx.send(Arbitration::BusUnavailable);
            }
        });
    match spawned {
        Ok(_) => ready_rx.recv().unwrap_or(Arbitration::BusUnavailable),
        Err(e) => {
            tracing::warn!(error = %e, "instance thread failed to spawn");
            Arbitration::BusUnavailable
        }
    }
}

/// Serve the instance object and try to acquire [`BUS_NAME`] as the
/// *sole* owner. `Ok(Some(conn))` means we own it (caller is primary);
/// `Ok(None)` means another prism already owns it. `.name()` on the
/// builder would silently queue and report success without ownership,
/// so we request the name explicitly with `DoNotQueue` and inspect the
/// reply.
fn serve(proxy: EventLoopProxy<HostCommand>) -> zbus::Result<Option<zbus::blocking::Connection>> {
    use zbus::fdo::{RequestNameFlags, RequestNameReply};

    // Register the object *before* claiming the name, so a handoff call
    // that arrives the instant we become owner finds the method present.
    let connection = zbus::blocking::connection::Builder::session()?
        .serve_at(OBJECT_PATH, Instance { proxy })?
        .build()?;

    let reply =
        connection.request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())?;
    match reply {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => Ok(Some(connection)),
        RequestNameReply::Exists | RequestNameReply::InQueue => Ok(None),
    }
}

/// Ask the running primary to open a browser window at `dir`. Retries
/// briefly to cover the window where the primary is still mid-startup
/// (two near-simultaneous launches).
pub fn hand_off(dir: &Path) -> zbus::Result<()> {
    let connection = zbus::blocking::Connection::session()?;
    let bytes = dir.as_os_str().as_bytes().to_vec();

    let mut last_err = None;
    for attempt in 0..5 {
        match connection.call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(BUS_NAME),
            "OpenWindow",
            &(bytes.clone(),),
        ) {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::debug!(attempt, error = %e, "hand_off retrying");
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last_err.expect("loop ran at least once"))
}
