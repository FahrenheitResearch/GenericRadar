//! Warning polling, off the egui thread.
//!
//! One worker owns the cadence. It fetches, sends what it got, then waits on
//! its refresh channel for the poll interval -- so a refresh asked for by the
//! analyst is served immediately rather than at the next tick.
//!
//! The source is fixed for the run, chosen from the command line or the
//! environment. It needs no runtime switch because the default source probes
//! for a daemon on every poll: start `nwws serve` while the workstation is
//! open and the next poll picks it up on its own.
//!
//! The egui thread only drains results. Nothing here blocks it, and a failed
//! poll arrives as a state to show rather than as an error to handle.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use data_source::warnings::{WarningRecord, WarningsSource, WarningsState, fetch_warnings};
use eframe::egui;

/// Refresh cadence. Warnings change on the scale of minutes, and both sources
/// are cheap to ask.
const POLL_INTERVAL: Duration = Duration::from_secs(45);

/// What one poll produced.
pub struct WarningsUpdate {
    pub state: WarningsState,
    /// `None` when the poll failed, which leaves the previous records on the
    /// map rather than blanking it on one bad round trip.
    pub records: Option<Vec<WarningRecord>>,
}

/// Ask for a poll now, rather than at the next tick.
struct RefreshNow;

pub struct WarningsService {
    refreshes: Sender<RefreshNow>,
    updates: Receiver<WarningsUpdate>,
}

impl WarningsService {
    /// Start polling `source` immediately.
    pub fn new(context: egui::Context, source: WarningsSource) -> Self {
        let (refresh_sender, refresh_receiver) = mpsc::channel();
        let (update_sender, update_receiver) = mpsc::channel();
        let _worker = thread::Builder::new()
            .name("warnings-poll".to_owned())
            .spawn(move || {
                poll_loop(context, source, &refresh_receiver, &update_sender);
            })
            .expect("failed to start warnings worker");
        Self {
            refreshes: refresh_sender,
            updates: update_receiver,
        }
    }

    /// Ask now, regardless of the cadence.
    pub fn refresh(&self) {
        let _ = self.refreshes.send(RefreshNow);
    }

    pub fn try_recv(&self) -> Option<WarningsUpdate> {
        self.updates.try_recv().ok()
    }
}

fn poll_loop(
    context: egui::Context,
    source: WarningsSource,
    refreshes: &Receiver<RefreshNow>,
    updates: &Sender<WarningsUpdate>,
) {
    loop {
        let fetched = fetch_warnings(&source);
        let delivered = updates
            .send(WarningsUpdate {
                state: fetched.state,
                records: fetched.records,
            })
            .is_ok();
        if !delivered {
            // The application is gone; nobody is listening.
            return;
        }
        context.request_repaint();

        match refreshes.recv_timeout(POLL_INTERVAL) {
            // Asked for, or the cadence elapsed: either way, poll again.
            Ok(RefreshNow) | Err(RecvTimeoutError::Timeout) => {}
            // The application dropped the service.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
