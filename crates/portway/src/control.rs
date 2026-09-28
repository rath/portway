//! The router a running forwarder serves, and the one way it is replaced.
//!
//! SIGHUP in daemon mode and the web console's reload button both end up in
//! [`reload`], so the two cannot drift apart: the same file is reread with the
//! same command-line overrides, the new upstreams inherit the old origin
//! state and are negotiated before any request is routed to them. The prices
//! a dashboard bills against are published by the same reload, because a
//! console that kept the ones it started with would price a route the file no
//! longer names — or refuse to price one it just added.
use std::sync::Arc;

use portway_core::Router;
use tokio::sync::{Mutex, RwLock};

use crate::cli::Args;
use crate::config::{Config, Prices};

/// What the listener reads on every request; swapped whole on a reload.
pub type RouterCell = Arc<RwLock<Arc<Router>>>;

/// What a dashboard reads when it turns tokens into money. Swapped whole, by
/// the same reload that swaps the router.
pub type PricesCell = Arc<RwLock<Arc<Prices>>>;

pub fn cell(router: &Arc<Router>) -> RouterCell {
    Arc::new(RwLock::new(Arc::clone(router)))
}

pub fn prices(prices: &Prices) -> PricesCell {
    Arc::new(RwLock::new(Arc::new(prices.clone())))
}

/// The router requests are routed to right now.
pub async fn current(cell: &RouterCell) -> Arc<Router> {
    Arc::clone(&*cell.read().await)
}

/// The prices a request is billed against right now.
pub async fn current_prices(cell: &PricesCell) -> Arc<Prices> {
    Arc::clone(&*cell.read().await)
}

/// Rebuild the router from the configuration and publish it, returning the
/// number of routes. On an error the running router is left untouched.
/// Reloads are serialized, so two that overlap cannot publish out of order.
pub async fn reload(args: &Args, cell: &RouterCell, prices: &PricesCell) -> Result<usize, String> {
    static RELOADING: Mutex<()> = Mutex::const_new(());
    let _one_at_a_time = RELOADING.lock().await;
    let config = Config::load(args)?;
    let next = config.router(args.mode)?;
    next.inherit_origin_state(&*current(cell).await);
    next.negotiate_all().await;
    let routes = next.models().len();
    *cell.write().await = next;
    *prices.write().await = Arc::new(config.prices);
    Ok(routes)
}
