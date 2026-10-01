//! A UDP Channel's bind scope in words (§15, ADR-047): who can reach a socket
//! bound to its address. "0.0.0.0" does not tell a reader that the socket is
//! reachable from the network; these words do.

use std::net::{IpAddr, Ipv4Addr};

/// The bind address that means every interface.
pub const ALL_INTERFACES_ADDRESS: &str = "0.0.0.0";

/// How the editor labels binding to every interface (§15).
pub const ALL_INTERFACES: &str = "All interfaces (reachable from the network)";

/// Who can reach a socket bound to an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindScope {
    /// Every interface, so anything on any network this computer is on.
    AllInterfaces,
    /// A loopback address: programs on this computer only.
    ThisComputer,
    /// One interface's address: what can reach that interface.
    OneInterface,
    /// Not an IP address. The Channel cannot start, and says why when it tries.
    Unreadable,
}

impl BindScope {
    /// The scope of a configured bind address. An IPv6 address may be written
    /// with the brackets the socket address needs.
    pub fn of(bind_address: &str) -> Self {
        let address = bind_address.trim();
        let address = address
            .strip_prefix('[')
            .and_then(|a| a.strip_suffix(']'))
            .unwrap_or(address);
        match address.parse::<IpAddr>() {
            Ok(ip) if ip.is_unspecified() => Self::AllInterfaces,
            Ok(ip) if ip.is_loopback() => Self::ThisComputer,
            Ok(_) => Self::OneInterface,
            Err(_) => Self::Unreadable,
        }
    }

    /// The scope for the Channel header, or `None` when the address is
    /// unreadable.
    pub fn words(self) -> Option<&'static str> {
        match self {
            Self::AllInterfaces => Some("all interfaces (reachable from the network)"),
            Self::ThisComputer => Some("this computer only"),
            Self::OneInterface => Some("one interface only"),
            Self::Unreadable => None,
        }
    }
}

/// The host's local addresses, as the driver last listed them.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum LocalAddresses {
    /// Not listed yet.
    #[default]
    NotListed,
    Listed(Vec<LocalAddress>),
    /// The OS could not list them, and why.
    Failed(String),
}

impl LocalAddresses {
    /// The listed addresses, or `None` while there is no list.
    pub fn listed(&self) -> Option<&[LocalAddress]> {
        match self {
            Self::Listed(addresses) => Some(addresses),
            Self::NotListed | Self::Failed(_) => None,
        }
    }
}

/// A local IPv4 address and the name the OS gives its interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAddress {
    pub ip: Ipv4Addr,
    pub interface: String,
}

/// The host's local IPv4 addresses on interfaces that are up, loopback last.
/// IPv4 only: broadcast has no IPv6 form, and an IPv6 bind address can still
/// be written into a profile. Asks the OS, so the driver calls it, never the
/// UI thread.
pub fn local_addresses() -> Result<Vec<LocalAddress>, String> {
    let interfaces = if_addrs::get_if_addrs().map_err(|err| err.to_string())?;
    Ok(in_choice_order(
        interfaces
            .into_iter()
            .filter(|interface| interface.is_oper_up())
            .filter_map(|interface| match interface.ip() {
                IpAddr::V4(ip) => Some(LocalAddress {
                    ip,
                    interface: interface.name,
                }),
                IpAddr::V6(_) => None,
            })
            .collect(),
    ))
}

/// Sorted by address with loopback last, each address once.
fn in_choice_order(mut addresses: Vec<LocalAddress>) -> Vec<LocalAddress> {
    addresses.sort_by_key(|address| (address.ip.is_loopback(), address.ip));
    addresses.dedup_by_key(|address| address.ip);
    addresses
}

/// The editor's bind choices, as (bind address, label): every interface
/// first, then each local address. The configured address is always among
/// them, so it is never silently replaced; once the addresses are listed, one
/// that is not among them says so. `listed` is `None` until they are.
pub fn bind_choices(configured: &str, listed: Option<&[LocalAddress]>) -> Vec<(String, String)> {
    let mut choices = vec![(ALL_INTERFACES_ADDRESS.to_owned(), ALL_INTERFACES.to_owned())];
    for address in listed.unwrap_or_default() {
        let label = if address.ip.is_loopback() {
            format!("{} \u{2014} this computer only", address.ip)
        } else {
            format!("{} \u{2014} {}", address.ip, address.interface)
        };
        choices.push((address.ip.to_string(), label));
    }
    let configured = configured.trim();
    if !configured.is_empty() && !choices.iter().any(|(address, _)| address == configured) {
        let label = match listed {
            Some(_) => format!("{configured} \u{2014} not on this computer now"),
            None => configured.to_owned(),
        };
        choices.push((configured.to_owned(), label));
    }
    choices
}

/// The label the closed bind choice shows for the configured address.
pub fn bind_choice_label(configured: &str, listed: Option<&[LocalAddress]>) -> String {
    let configured = configured.trim();
    if configured.is_empty() {
        return "select address\u{2026}".to_owned();
    }
    bind_choices(configured, listed)
        .into_iter()
        .find(|(address, _)| address == configured)
        .map_or_else(|| configured.to_owned(), |(_, label)| label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(ip: [u8; 4], interface: &str) -> LocalAddress {
        LocalAddress {
            ip: Ipv4Addr::from(ip),
            interface: interface.to_owned(),
        }
    }

    #[test]
    fn the_scope_says_who_can_reach_the_socket() {
        assert_eq!(BindScope::of("0.0.0.0"), BindScope::AllInterfaces);
        assert_eq!(BindScope::of(" 0.0.0.0 "), BindScope::AllInterfaces);
        assert_eq!(BindScope::of("[::]"), BindScope::AllInterfaces);
        assert_eq!(BindScope::of("127.0.0.1"), BindScope::ThisComputer);
        assert_eq!(BindScope::of("[::1]"), BindScope::ThisComputer);
        assert_eq!(BindScope::of("192.168.1.20"), BindScope::OneInterface);
        assert_eq!(BindScope::of("not-an-address"), BindScope::Unreadable);
        assert_eq!(BindScope::of(""), BindScope::Unreadable);

        assert_eq!(
            BindScope::AllInterfaces.words(),
            Some("all interfaces (reachable from the network)")
        );
        assert_eq!(BindScope::ThisComputer.words(), Some("this computer only"));
        assert_eq!(BindScope::OneInterface.words(), Some("one interface only"));
        assert_eq!(BindScope::Unreadable.words(), None);
    }

    #[test]
    fn local_addresses_are_ordered_with_loopback_last_and_each_once() {
        let ordered = in_choice_order(vec![
            address([127, 0, 0, 1], "Loopback Pseudo-Interface 1"),
            address([192, 168, 1, 20], "Ethernet"),
            address([10, 0, 0, 5], "Wi-Fi"),
            address([192, 168, 1, 20], "Ethernet 2"),
        ]);
        let ips: Vec<String> = ordered.iter().map(|a| a.ip.to_string()).collect();
        assert_eq!(ips, ["10.0.0.5", "192.168.1.20", "127.0.0.1"]);
    }

    #[test]
    fn the_choices_name_every_interface_and_each_address_interface() {
        let listed = [
            address([192, 168, 1, 20], "Ethernet"),
            address([127, 0, 0, 1], "lo"),
        ];
        assert_eq!(
            bind_choices("0.0.0.0", Some(&listed)),
            [
                ("0.0.0.0".to_owned(), ALL_INTERFACES.to_owned()),
                (
                    "192.168.1.20".to_owned(),
                    "192.168.1.20 \u{2014} Ethernet".to_owned()
                ),
                (
                    "127.0.0.1".to_owned(),
                    "127.0.0.1 \u{2014} this computer only".to_owned()
                ),
            ]
        );
        assert_eq!(bind_choice_label("0.0.0.0", Some(&listed)), ALL_INTERFACES);
        assert_eq!(
            bind_choice_label("192.168.1.20", Some(&listed)),
            "192.168.1.20 \u{2014} Ethernet"
        );
    }

    #[test]
    fn a_configured_address_is_kept_and_says_when_it_is_not_here() {
        let listed = [address([192, 168, 1, 20], "Ethernet")];
        let choices = bind_choices("10.9.9.9", Some(&listed));
        assert_eq!(
            choices.last(),
            Some(&(
                "10.9.9.9".to_owned(),
                "10.9.9.9 \u{2014} not on this computer now".to_owned()
            ))
        );
        assert_eq!(
            bind_choice_label("10.9.9.9", Some(&listed)),
            "10.9.9.9 \u{2014} not on this computer now"
        );
        // Before the list arrives, nothing is known to be missing.
        assert_eq!(bind_choice_label("10.9.9.9", None), "10.9.9.9");
        assert_eq!(bind_choices("10.9.9.9", None).len(), 2);
        // An empty address asks for a choice rather than showing nothing.
        assert_eq!(
            bind_choice_label("", Some(&listed)),
            "select address\u{2026}"
        );
    }

    #[test]
    fn this_computer_lists_its_loopback_address() {
        // Asks the real OS: every host this runs on has a loopback interface up.
        let listed = local_addresses().expect("the OS lists its addresses");
        assert!(
            listed.iter().any(|a| a.ip == Ipv4Addr::LOCALHOST),
            "{listed:?}"
        );
        assert!(
            listed.last().is_some_and(|a| a.ip.is_loopback()),
            "{listed:?}"
        );
    }
}
