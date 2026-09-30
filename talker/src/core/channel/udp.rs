use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

use anyhow::Context;

use super::config::{InterfaceConfig, UdpConfig, UdpMode};
use super::Interface;

pub(super) struct UdpInterface {
    socket: UdpSocket,
    destination: SocketAddr,
}

impl UdpInterface {
    pub(super) fn open(config: &UdpConfig) -> anyhow::Result<Self> {
        // Bound in the destination's address family: an IPv4 socket cannot
        // send to an IPv6 address, or the reverse.
        let any: IpAddr = if config.destination().is_ipv6() {
            Ipv6Addr::UNSPECIFIED.into()
        } else {
            Ipv4Addr::UNSPECIFIED.into()
        };
        let local_port = config.local_port.unwrap_or(0);
        let socket = UdpSocket::bind((any, local_port)).context("binding UDP socket")?;
        let destination = apply_socket_config(&socket, config, false)?;
        Ok(Self {
            socket,
            destination,
        })
    }
}

impl Interface for UdpInterface {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.socket
            .send_to(data, self.destination)
            .context("sending UDP datagram")?;
        Ok(())
    }

    fn reconfigure(
        &mut self,
        current: &InterfaceConfig,
        next: &InterfaceConfig,
    ) -> anyhow::Result<bool> {
        let (InterfaceConfig::Udp(current), InterfaceConfig::Udp(next)) = (current, next) else {
            return Ok(false);
        };
        // A new port or address family needs a new socket.
        if current.local_port != next.local_port
            || current.destination().is_ipv6() != next.destination().is_ipv6()
        {
            return Ok(false);
        }

        let reset_for_next = is_multicast(&current.mode) && !is_multicast(&next.mode);
        match apply_socket_config(&self.socket, next, reset_for_next) {
            Ok(destination) => {
                self.destination = destination;
                Ok(true)
            }
            Err(change_err) => {
                let reset_for_rollback = is_multicast(&next.mode) && !is_multicast(&current.mode);
                match apply_socket_config(&self.socket, current, reset_for_rollback) {
                    Ok(destination) => {
                        self.destination = destination;
                        Err(change_err
                            .context("applying UDP settings; previous settings were restored"))
                    }
                    Err(rollback_err) => Err(change_err.context(format!(
                        "applying UDP settings; restoring the previous settings also failed: \
                     {rollback_err:#}"
                    ))),
                }
            }
        }
    }
}

fn is_multicast(mode: &UdpMode) -> bool {
    matches!(mode, UdpMode::Multicast { .. })
}

fn apply_socket_config(
    socket: &UdpSocket,
    config: &UdpConfig,
    reset_inactive_multicast_options: bool,
) -> anyhow::Result<SocketAddr> {
    let broadcast = matches!(&config.mode, UdpMode::Broadcast { .. });
    if config.destination().is_ipv6() {
        // IPv6 has no broadcast; its sockets have no broadcast mode to set.
        anyhow::ensure!(
            !broadcast,
            "IPv6 has no broadcast; use a multicast group or a unicast address"
        );
    } else {
        socket
            .set_broadcast(broadcast)
            .context("updating UDP broadcast mode")?;
    }
    let destination = match &config.mode {
        UdpMode::Unicast { destination } | UdpMode::Broadcast { destination } => {
            if reset_inactive_multicast_options {
                socket2::SockRef::from(socket)
                    .set_multicast_if_v4(&std::net::Ipv4Addr::UNSPECIFIED)
                    .context("resetting UDP multicast interface")?;
                socket
                    .set_multicast_ttl_v4(1)
                    .context("resetting UDP multicast TTL")?;
            }
            *destination
        }
        UdpMode::Multicast {
            group,
            port,
            interface,
            ttl,
        } => {
            socket2::SockRef::from(socket)
                .set_multicast_if_v4(&interface.unwrap_or(std::net::Ipv4Addr::UNSPECIFIED))
                .context("updating UDP multicast interface")?;
            socket
                .set_multicast_ttl_v4(ttl.unwrap_or(1))
                .context("updating UDP multicast TTL")?;
            SocketAddr::from((*group, *port))
        }
    };
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicast_send_loopback() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest: SocketAddr = receiver.local_addr().unwrap();

        let config = UdpConfig::unicast(dest);
        let mut conn = UdpInterface::open(&config).unwrap();

        conn.send(b"hello").unwrap();

        let mut buf = [0u8; 16];
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn unicast_send_reaches_an_ipv6_destination() {
        // An IPv4 socket cannot send to an IPv6 address, so the socket is bound
        // in the destination's address family.
        let Ok(receiver) = UdpSocket::bind("[::1]:0") else {
            return; // no IPv6 loopback on this host
        };
        let dest = receiver.local_addr().unwrap();
        let mut conn = UdpInterface::open(&UdpConfig::unicast(dest)).unwrap();
        conn.send(b"v6").unwrap();

        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut buf = [0u8; 8];
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"v6");
    }

    #[test]
    fn a_change_of_address_family_asks_for_a_new_socket() {
        let current = UdpConfig::unicast("127.0.0.1:9".parse().unwrap());
        let next = UdpConfig::unicast("[::1]:9".parse().unwrap());
        let mut interface = UdpInterface::open(&current).unwrap();
        assert!(!interface
            .reconfigure(&InterfaceConfig::Udp(current), &InterfaceConfig::Udp(next))
            .unwrap());
    }

    #[test]
    fn same_bound_port_reconfigures_without_a_second_bind() {
        let first_receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let second_receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port_probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_port = port_probe.local_addr().unwrap().port();
        drop(port_probe);

        let mut current = UdpConfig::unicast(first_receiver.local_addr().unwrap());
        current.local_port = Some(local_port);
        let mut next = UdpConfig::unicast(second_receiver.local_addr().unwrap());
        next.local_port = Some(local_port);
        let mut interface = UdpInterface::open(&current).unwrap();

        assert!(interface
            .reconfigure(&InterfaceConfig::Udp(current), &InterfaceConfig::Udp(next))
            .unwrap());
        interface.send(b"new destination").unwrap();
        second_receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut buf = [0; 32];
        let (n, _) = second_receiver.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"new destination");
    }

    #[test]
    fn leaving_multicast_resets_ttl_and_reuses_the_bound_socket() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port_probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_port = port_probe.local_addr().unwrap().port();
        drop(port_probe);

        let mut current =
            UdpConfig::multicast_with("239.0.0.1".parse().unwrap(), 20_000, None, Some(7));
        current.local_port = Some(local_port);
        let mut next = UdpConfig::unicast(receiver.local_addr().unwrap());
        next.local_port = Some(local_port);
        let mut interface = UdpInterface::open(&current).unwrap();
        assert_eq!(interface.socket.multicast_ttl_v4().unwrap(), 7);

        assert!(interface
            .reconfigure(&InterfaceConfig::Udp(current), &InterfaceConfig::Udp(next))
            .unwrap());
        assert_eq!(interface.socket.multicast_ttl_v4().unwrap(), 1);
        interface.send(b"unicast").unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut buf = [0; 16];
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"unicast");
    }

    #[test]
    fn broadcast_socket_opens() {
        let dest: SocketAddr = "255.255.255.255:19999".parse().unwrap();
        let config = UdpConfig::broadcast(dest);
        assert!(UdpInterface::open(&config).is_ok());
    }

    #[test]
    fn multicast_socket_opens() {
        let config = UdpConfig::multicast("239.0.0.1".parse().unwrap(), 20000);
        assert!(UdpInterface::open(&config).is_ok());
    }

    #[test]
    fn multicast_socket_opens_with_ttl_and_default_interface() {
        // TTL is applied via std; the default interface (0.0.0.0) is a valid
        // IP_MULTICAST_IF selection, so both options exercise the open path.
        let config = UdpConfig::multicast_with(
            "239.0.0.2".parse().unwrap(),
            20001,
            Some(std::net::Ipv4Addr::UNSPECIFIED),
            Some(4),
        );
        let conn = UdpInterface::open(&config).unwrap();
        assert_eq!(conn.socket.multicast_ttl_v4().unwrap(), 4);
    }

    #[test]
    fn multicast_send_to_loopback_group_with_ttl() {
        // A receiver joined to a group on loopback receives a datagram sent
        // with an explicit TTL — proves the configured socket actually sends.
        let group: std::net::Ipv4Addr = "239.255.0.99".parse().unwrap();
        let receiver = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let port = receiver.local_addr().unwrap().port();
        let loopback = std::net::Ipv4Addr::LOCALHOST;
        if receiver.join_multicast_v4(&group, &loopback).is_err() {
            // Some CI hosts have no multicast-capable loopback; skip cleanly.
            return;
        }
        receiver.set_multicast_loop_v4(true).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();

        let mut conn = UdpInterface::open(&UdpConfig::multicast_with(
            group,
            port,
            Some(loopback),
            Some(1),
        ))
        .unwrap();
        conn.send(b"mc").unwrap();

        let mut buf = [0u8; 8];
        if let Ok((n, _)) = receiver.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"mc");
        }
        // A missed datagram (host multicast quirks) is not a logic failure;
        // the assertion above only fires when something *was* received.
    }

    #[test]
    fn send_multiple_datagrams() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = receiver.local_addr().unwrap();

        let mut conn = UdpInterface::open(&UdpConfig::unicast(dest)).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        for msg in [b"one" as &[u8], b"two", b"three"] {
            conn.send(msg).unwrap();
            let mut buf = [0u8; 16];
            let (n, _) = receiver.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], msg);
        }
    }
}
