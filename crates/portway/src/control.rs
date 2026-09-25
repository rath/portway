//! The router a running forwarder serves, and the one way it is replaced.
//!
//! SIGHUP in daemon mode and the web console's reload button both end up in
//! [`reload`], so the two cannot drift apart: the same file is reread with the
//! same command-line overrides, the new upstreams inherit the old origin
//! state and are negotiated before any request is routed to them.
use std::sync::Arc;

use portway_core::Router;
use tokio::sync::{Mutex, RwLock};

use crate::cli::Args;
use crate::config::Config;

/// What the listener reads on every request; swapped whole on a reload.
pub type RouterCell = Arc<RwLock<Arc<Router>>>;

pub fn cell(router: &Arc<Router>) -> RouterCell {
    Arc::new(RwLock::new(Arc::clone(router)))
}

/// The router requests are routed to right now.
pub async fn current(cell: &RouterCell) -> Arc<Router> {
    Arc::clone(&*cell.read().await)
}

/// Rebuild the router from the configuration and publish it, returning the
/// number of routes. On an error the running router is left untouched.
/// Reloads are serialized, so two that overlap cannot publish out of order.
pub async fn reload(args: &Args, cell: &RouterCell) -> Result<usize, String> {
    static RELOADING: Mutex<()> = Mutex::const_new(());
    let _one_at_a_time = RELOADING.lock().await;
    let next = Config::load(args).and_then(|config| config.router(args.mode))?;
    next.inherit_origin_state(&*current(cell).await);
    next.negotiate_all().await;
    let routes = next.models().len();
    *cell.write().await = next;
    Ok(routes)
}
