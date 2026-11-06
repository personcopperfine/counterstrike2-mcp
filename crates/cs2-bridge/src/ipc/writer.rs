//! The writer thread: the outbound queue's only reader.
//!
//! Port of `writer_thread` (`mcp_bridge.c:1539-1562`): pop with a 100 ms wake interval
//! (`out_pop_blocking(&o, 100)`), deliver only to the connection whose generation produced the
//! record (`o.gen == g_clientGen && g_authedClient != INVALID_SOCKET`), and log a send failure
//! or a stale-generation drop. The thread exits when a pop finds nothing and the bridge is
//! shutting down (the C's `g_shuttingDown`).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::log;
use crate::queue;

use super::listener::{Delivery, Shared};

/// The C's pop timeout (`mcp_bridge.c:1545`, `out_pop_blocking(&o, 100)`).
const POP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// Runs the writer until the bridge shuts down; spawned by [`super::start`].
pub(crate) fn run(shared: Arc<Shared>) {
    log::line("writer: thread start");
    loop {
        match queue::global().out_pop_blocking(POP_TIMEOUT) {
            Some((line, gen)) => match shared.deliver(gen, line.as_bytes()) {
                Delivery::Sent => {}
                Delivery::Failed => log::line("writer: send failed"),
                Delivery::Stale => {
                    log::line(&format!(
                        "writer: dropped record for stale generation {gen}"
                    ));
                }
            },
            None => {
                // The C: `if (!out_pop_blocking(&o, 100)) { if (g_shuttingDown) break; continue; }`
                // (`:1545-1547`).
                if shared.shutdown.load(Ordering::Acquire) || queue::global().is_shutting_down() {
                    break;
                }
            }
        }
    }
    log::line("writer: thread exit");
}
