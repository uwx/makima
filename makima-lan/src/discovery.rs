//! Announcing a shared session on the local network, so a window finds it
//! without being told an address. The token is never announced: reaching the
//! session still needs it.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

use mdns_sd::{Receiver, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info, warn};

use crate::reachable;

/// What a client browses for.
pub const SERVICE_TYPE: &str = "_makima._tcp.local.";
/// TXT: the directory the session works in.
pub const PROJECT_KEY: &str = "project";
/// TXT: the discovery format, so a future client can tell an older agent.
pub const VERSION_KEY: &str = "version";
pub const VERSION: &str = "1";

const UNNAMED_PROJECT: &str = "session";
const UNNAMED_MACHINE: &str = "this machine";
const FALLBACK_LABEL: &str = "makima";

/// A session another machine offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The announced full name, which is how removals name the session.
    pub fullname: String,
    /// The name a window shows, for example `makima on box`.
    pub name: String,
    /// The directory the session works in.
    pub project: String,
    /// Where to connect.
    pub address: SocketAddr,
}

/// What browsing the network reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovered {
    Found(Found),
    /// The session named {0} stopped being offered.
    Gone(String),
}

/// One live announcement. Dropping it withdraws the service.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    /// Announces the listener at {bound} for {project}. Answers `None` when the
    /// network offers no discovery: a client that knows the address still
    /// reaches the session.
    pub fn start(bound: SocketAddr, project: &Path) -> Option<Self> {
        let address = reachable(bound);
        if address.ip().is_loopback() {
            debug!("not announcing a listener only this machine can reach");
            return None;
        }
        let port = address.port();
        let project = project_name(project);
        let machine = machine_name();
        let name = format!("{project} on {machine}");
        let host = format!("{}-{port}.local.", dns_label(&machine));
        let properties = [(VERSION_KEY, VERSION), (PROJECT_KEY, project.as_str())];
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &name,
            &host,
            address.ip(),
            port,
            &properties[..],
        );
        let info = match service {
            Ok(info) => info,
            Err(error) => {
                warn!(%error, "cannot announce the shared session");
                return None;
            }
        };
        let daemon = match ServiceDaemon::new() {
            Ok(daemon) => daemon,
            Err(error) => {
                warn!(%error, "cannot start local network discovery");
                return None;
            }
        };
        let fullname = info.get_fullname().to_owned();
        if let Err(error) = daemon.register(info) {
            warn!(%error, "cannot announce the shared session");
            let _ = daemon.shutdown();
            return None;
        }
        info!(%name, port, "announced the shared session on the local network");
        Some(Self { daemon, fullname })
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Searches the local network for sessions. Dropping it stops searching.
pub struct Browser {
    daemon: ServiceDaemon,
    events: Receiver<ServiceEvent>,
}

impl Browser {
    pub fn start() -> Option<Self> {
        let daemon = match ServiceDaemon::new() {
            Ok(daemon) => daemon,
            Err(error) => {
                warn!(%error, "cannot start local network discovery");
                return None;
            }
        };
        let events = match daemon.browse(SERVICE_TYPE) {
            Ok(events) => events,
            Err(error) => {
                warn!(%error, "cannot search the local network");
                let _ = daemon.shutdown();
                return None;
            }
        };
        Some(Self { daemon, events })
    }

    /// Waits up to {timeout} for a session to appear or go away. Answers
    /// `None` when nothing happened, or when searching stopped.
    pub fn next(&self, timeout: Duration) -> Option<Discovered> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.events.recv_timeout(left).ok()? {
                ServiceEvent::ServiceResolved(info) => {
                    if let Some(found) = found(&info) {
                        return Some(Discovered::Found(found));
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    return Some(Discovered::Gone(fullname));
                }
                _ => {}
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

fn found(info: &ResolvedService) -> Option<Found> {
    let address = SocketAddr::new(preferred_address(&info.addresses)?, info.port);
    Some(Found {
        fullname: info.fullname.clone(),
        name: instance_name(&info.fullname).to_owned(),
        project: info
            .txt_properties
            .get_property_val_str(PROJECT_KEY)
            .unwrap_or_default()
            .to_owned(),
        address,
    })
}

/// The address to connect to: IPv4 first and not loopback, so a session on this
/// machine is still reached over an interface a peer could use.
fn preferred_address(addresses: &HashSet<ScopedIp>) -> Option<IpAddr> {
    let known: Vec<IpAddr> = addresses.iter().map(ScopedIp::to_ip_addr).collect();
    known
        .iter()
        .copied()
        .find(|ip| ip.is_ipv4() && !ip.is_loopback())
        .or_else(|| known.iter().copied().find(|ip| ip.is_ipv4()))
        .or_else(|| known.first().copied())
}

/// `makima on box._makima._tcp.local.` names the instance `makima on box`.
fn instance_name(fullname: &str) -> &str {
    fullname
        .strip_suffix(&format!(".{SERVICE_TYPE}"))
        .unwrap_or(fullname)
}

fn project_name(project: &Path) -> String {
    project
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(UNNAMED_PROJECT)
        .to_owned()
}

fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .map(|name| name.trim().to_owned())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| UNNAMED_MACHINE.to_owned())
}

/// A name a remote machine can put in a DNS record.
fn dns_label(name: &str) -> String {
    let label: String = name
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let label = label.trim_matches('-');
    if label.is_empty() {
        FALLBACK_LABEL.to_owned()
    } else {
        label.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case("makima on box._makima._tcp.local." => "makima on box"; "the service suffix goes")]
    #[test_case("other name" => "other name"; "a name without the suffix is kept")]
    fn the_instance_name_drops_the_service_suffix(fullname: &str) -> String {
        instance_name(fullname).to_owned()
    }

    #[test_case("Box-PC" => "box-pc"; "case folds")]
    #[test_case("box.local" => "box-local"; "dots become hyphens")]
    #[test_case("--weird--" => "weird"; "dashes are trimmed")]
    #[test_case("Ünïcode" => "n-code"; "only ascii letters and digits stay")]
    #[test_case("Üü" => "makima"; "a name with nothing left falls back")]
    fn a_machine_name_becomes_a_dns_label(name: &str) -> String {
        dns_label(name)
    }

    #[test]
    fn a_directory_without_a_name_is_still_announced() {
        assert_eq!(project_name(Path::new("/")), UNNAMED_PROJECT);
        assert_eq!(project_name(Path::new("/home/makima/app")), "app");
    }

    use std::net::Ipv4Addr;

    use test_case::test_case;

    #[test]
    fn the_address_to_connect_to_is_one_a_peer_could_use() {
        let lan = Ipv4Addr::new(192, 168, 2, 4);
        let addresses = [Ipv4Addr::LOCALHOST, lan]
            .into_iter()
            .map(|ip| ScopedIp::from(IpAddr::V4(ip)))
            .collect();
        assert_eq!(preferred_address(&addresses), Some(IpAddr::V4(lan)));
    }

    #[test]
    fn an_announcement_of_loopback_only_still_resolves() {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let addresses = [ScopedIp::from(loopback)].into_iter().collect();
        assert_eq!(preferred_address(&addresses), Some(loopback));
    }

    /// Needs an interface the network multicasts on, as sharing does.
    #[test]
    fn a_shared_session_is_findable_and_withdrawn() {
        let bound = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 45871));
        let advertisement = Advertisement::start(bound, Path::new("/tmp/probe-project"))
            .expect("a listener on every interface is announced");
        let browser = Browser::start().expect("browsing starts");
        let found = wait_for(&browser, |discovered| match discovered {
            Discovered::Found(found) if found.project == PROBE_PROJECT => Some(found.clone()),
            _ => None,
        })
        .expect("the announcement is found");
        assert_eq!(found.address.port(), 45871);
        assert!(!found.address.ip().is_loopback(), "{found:?}");
        assert!(
            found.name.starts_with(&format!("{PROBE_PROJECT} on ")),
            "{found:?}"
        );

        drop(advertisement);
        let gone = wait_for(&browser, |discovered| match discovered {
            Discovered::Gone(name) => Some(name.clone()),
            _ => None,
        })
        .expect("the announcement is withdrawn");
        assert_eq!(gone, found.fullname);
    }

    const PROBE_PROJECT: &str = "probe-project";

    /// Waits up to a few seconds for the network to answer.
    fn wait_for<T>(
        browser: &Browser,
        mut accept: impl FnMut(&Discovered) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(accepted) = accept(&browser.next(WAIT)?) {
                return Some(accepted);
            }
        }
        None
    }

    const WAIT: Duration = Duration::from_secs(2);
}
