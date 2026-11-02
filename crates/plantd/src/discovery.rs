//! Finds nodes via mDNS (`_soilnode._tcp`). The browser runs on mdns-sd's own thread;
//! the poller drains whatever it resolved once per cycle without blocking.

use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};

use crate::config;
use crate::store::valid_id;

#[derive(Debug)]
pub struct Found {
    pub addr: String,
    /// From the TXT record. Without it the node's id is learned on first poll.
    pub id: Option<String>,
    pub hostname: Option<String>,
}

pub struct Discovery {
    // Dropping the daemon stops the browse, so keep it alive alongside the receiver.
    browse: Option<(ServiceDaemon, Receiver<ServiceEvent>)>,
}

impl Discovery {
    pub fn start(cfg: &config::Discovery) -> Self {
        if !cfg.mdns {
            return Self { browse: None };
        }
        let browse = ServiceDaemon::new()
            .and_then(|d| d.browse(&cfg.service).map(|rx| (d, rx)))
            .inspect(|_| log::info!("mDNS browsing {}", cfg.service))
            .inspect_err(|e| log::error!("mDNS disabled: {e}"))
            .ok();
        Self { browse }
    }

    /// Everything resolved since the last call.
    pub fn drain(&self) -> Vec<Found> {
        let Some((_, rx)) = &self.browse else {
            return Vec::new();
        };
        rx.try_iter()
            .filter_map(|ev| match ev {
                ServiceEvent::ServiceResolved(info) => {
                    let ip = info.get_addresses_v4().into_iter().min()?;
                    let found = Found {
                        addr: format!("{ip}:{}", info.get_port()),
                        id: info
                            .get_property_val_str("id")
                            .filter(|id| valid_id(id))
                            .map(str::to_owned),
                        hostname: Some(
                            info.host
                                .trim_end_matches('.')
                                .trim_end_matches(".local")
                                .to_owned(),
                        ),
                    };
                    log::debug!("mDNS resolved {found:?}");
                    Some(found)
                }
                _ => None,
            })
            .collect()
    }
}
