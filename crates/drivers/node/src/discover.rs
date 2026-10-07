//! Finding a node by the id it advertises over mDNS.
//!
//! The daemon lives for one lookup: discovery runs only when no address is known, so a thread
//! held for the life of the process would idle almost always.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use driver::ReadError;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::time::timeout;
use tracing::debug;

/// What a node advertises.
const SERVICE: &str = "_modbus._tcp.local.";
/// The TXT key carrying the node's id.
const ID_KEY: &str = "id";

/// Where the node advertising `node_id` listens, or [`ReadError::Timeout`] if none answers in
/// `limit`.
pub async fn find(node_id: &str, limit: Duration) -> Result<SocketAddr, ReadError> {
    let daemon = ServiceDaemon::new().map_err(|e| ReadError::Refused(e.to_string()))?;
    let found = find_on(&daemon, node_id, limit).await;
    if let Err(e) = daemon.shutdown() {
        debug!(error = %e, "mdns daemon did not shut down");
    }
    found
}

async fn find_on(
    daemon: &ServiceDaemon,
    node_id: &str,
    limit: Duration,
) -> Result<SocketAddr, ReadError> {
    let events = daemon
        .browse(SERVICE)
        .map_err(|e| ReadError::Refused(e.to_string()))?;
    let search = async {
        while let Ok(event) = events.recv_async().await {
            if let ServiceEvent::ServiceResolved(service) = event
                && let Some(address) = find_matching(
                    service.get_property_val_str(ID_KEY),
                    service.get_addresses_v4(),
                    service.get_port(),
                    node_id,
                )
            {
                return Some(address);
            }
        }
        None
    };
    timeout(limit, search)
        .await
        .ok()
        .flatten()
        .ok_or(ReadError::Timeout)
}

/// The address of a resolved service, when it is the node asked for. The lowest address of
/// several, so the same advert always gives the same answer.
fn find_matching(
    id: Option<&str>,
    addresses: impl IntoIterator<Item = Ipv4Addr>,
    port: u16,
    node_id: &str,
) -> Option<SocketAddr> {
    if id != Some(node_id) {
        return None;
    }
    let address = addresses.into_iter().min()?;
    Some(SocketAddr::from((address, port)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 7);

    #[test]
    fn the_node_asked_for_is_found_at_its_lowest_address() {
        let other = Ipv4Addr::new(10, 0, 0, 9);
        let found = find_matching(Some("garage"), [other, ADDRESS], 502, "garage");
        assert_eq!(found, Some(SocketAddr::from((ADDRESS, 502))));
    }

    #[test]
    fn another_node_is_not_taken_for_it() {
        assert_eq!(find_matching(Some("attic"), [ADDRESS], 502, "garage"), None);
    }

    #[test]
    fn an_advert_without_an_id_is_not_taken_for_it() {
        assert_eq!(find_matching(None, [ADDRESS], 502, "garage"), None);
    }

    #[test]
    fn an_advert_without_an_address_is_not_found() {
        assert_eq!(find_matching(Some("garage"), [], 502, "garage"), None);
    }
}
