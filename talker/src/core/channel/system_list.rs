//! Lists the operating system keeps, read away from the window thread: the
//! serial ports and this computer's network addresses.
//!
//! Listing ports asks the operating system, and through it each port's driver,
//! so a wedged driver can hold the call; listing addresses asks every network
//! adapter. Read on a helper thread, that holds only the helper, and the window
//! keeps showing the last list it has.

use std::cmp::Ordering;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

type Notify = Arc<dyn Fn() + Send + Sync>;
type Enumerate<T> = Arc<dyn Fn() -> Vec<T> + Send + Sync>;

/// The list and the read behind it.
///
/// **One read at a time.** A refresh asked for while a read runs is held, and
/// however many arrive, one more read follows when it ends: the newest request
/// is answered by a list read after it, and a wedged driver holds one helper
/// thread, not one per click.
struct State<T> {
    /// The newest list, sorted. `None` until the first read finishes.
    items: Option<Vec<T>>,
    /// Lists published so far, so a reader can tell a new one.
    reads: u64,
    /// A helper is reading.
    reading: bool,
    /// A refresh arrived while the helper was reading.
    again: bool,
}

impl<T> Default for State<T> {
    fn default() -> Self {
        Self {
            items: None,
            reads: 0,
            reading: false,
            again: false,
        }
    }
}

/// Cloneable handle on one list. [`refresh`](Self::refresh) starts a read on a
/// helper thread; [`newer_than`](Self::newer_than) hands back each new list
/// once, and the notify callback says when one is there.
pub(crate) struct SystemList<T> {
    /// What is listed, in words, for the helper thread's name and a warning.
    what: &'static str,
    state: Arc<Mutex<State<T>>>,
    notify: Arc<Mutex<Option<Notify>>>,
    enumerate: Enumerate<T>,
}

/// The serial ports, by name.
pub(crate) type PortList = SystemList<String>;

/// This computer's IPv4 addresses on network adapters that are up.
pub(crate) type AddressList = SystemList<LocalAddress>;

impl Default for PortList {
    fn default() -> Self {
        Self::new("serial ports", Arc::new(enumerate_serial_ports))
    }
}

impl Default for AddressList {
    fn default() -> Self {
        Self::new("network addresses", Arc::new(enumerate_local_addresses))
    }
}

impl<T> Clone for SystemList<T> {
    fn clone(&self) -> Self {
        Self {
            what: self.what,
            state: Arc::clone(&self.state),
            notify: Arc::clone(&self.notify),
            enumerate: Arc::clone(&self.enumerate),
        }
    }
}

impl<T: Clone + Ord + Send + 'static> SystemList<T> {
    fn new(what: &'static str, enumerate: Enumerate<T>) -> Self {
        Self {
            what,
            state: Arc::new(Mutex::new(State::default())),
            notify: Arc::new(Mutex::new(None)),
            enumerate,
        }
    }

    pub(crate) fn set_notify(&self, notify: Notify) {
        *lock(&self.notify) = Some(notify);
    }

    /// The newest list, and its read count, when it is newer than `seen`.
    pub(crate) fn newer_than(&self, seen: u64) -> Option<(u64, Vec<T>)> {
        let state = lock(&self.state);
        (state.reads > seen)
            .then(|| state.items.clone().map(|items| (state.reads, items)))
            .flatten()
    }

    /// Whether a read is under way, so the list on hand may be about to change.
    pub(crate) fn is_reading(&self) -> bool {
        lock(&self.state).reading
    }

    /// Read the list again, on a helper thread.
    pub(crate) fn refresh(&self) {
        {
            let mut state = lock(&self.state);
            if state.reading {
                state.again = true;
                return;
            }
            state.reading = true;
        }
        let state = Arc::clone(&self.state);
        let notify_slot = Arc::clone(&self.notify);
        let enumerate = Arc::clone(&self.enumerate);
        let spawned = std::thread::Builder::new()
            .name(format!("talker-{}", self.what.replace(' ', "-")))
            .spawn(move || loop {
                let mut items = enumerate();
                items.sort();
                let again = {
                    let mut state = lock(&state);
                    state.items = Some(items);
                    state.reads += 1;
                    let again = std::mem::take(&mut state.again);
                    state.reading = again;
                    again
                };
                notify(&notify_slot);
                if !again {
                    break;
                }
            });
        if let Err(error) = spawned {
            // The list on hand stays. The next refresh tries again.
            lock(&self.state).reading = false;
            tracing::warn!(
                "listing {}: the system refused a new thread: {error}",
                self.what
            );
        }
    }
}

/// The serial ports the operating system lists now. A failed listing reads as
/// no ports, as the port picker has always shown it.
fn enumerate_serial_ports() -> Vec<String> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|port| port.port_name)
        .collect()
}

/// A local IPv4 address and the name the system gives its network adapter.
///
/// Ordered by address with loopback last, so a sorted list puts the addresses
/// that reach other computers first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalAddress {
    pub ip: Ipv4Addr,
    pub interface: String,
}

impl Ord for LocalAddress {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.ip.is_loopback(), self.ip, &self.interface).cmp(&(
            other.ip.is_loopback(),
            other.ip,
            &other.interface,
        ))
    }
}

impl PartialOrd for LocalAddress {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// This computer's IPv4 addresses on adapters that are up, each address once.
/// IPv4 only, as talker's UDP is. A failed listing reads as no addresses and
/// is logged; the choices then hold only the system default and whatever the
/// profile names.
fn enumerate_local_addresses() -> Vec<LocalAddress> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => one_per_address(
            interfaces
                .into_iter()
                .filter(if_addrs::Interface::is_oper_up)
                .filter_map(|interface| match interface.ip() {
                    IpAddr::V4(ip) => Some(LocalAddress {
                        ip,
                        interface: interface.name,
                    }),
                    IpAddr::V6(_) => None,
                })
                .collect(),
        ),
        Err(error) => {
            tracing::warn!("listing network addresses: {error}");
            Vec::new()
        }
    }
}

/// Sorted, with an address that two adapters report kept once.
fn one_per_address(mut addresses: Vec<LocalAddress>) -> Vec<LocalAddress> {
    addresses.sort();
    addresses.dedup_by_key(|address| address.ip);
    addresses
}

fn notify(slot: &Mutex<Option<Notify>>) {
    let callback = lock(slot).clone();
    if let Some(callback) = callback {
        callback();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::*;

    fn notifications<T: Clone + Ord + Send + 'static>(
        list: &SystemList<T>,
    ) -> crossbeam_channel::Receiver<()> {
        let (tx, rx) = crossbeam_channel::unbounded();
        list.set_notify(Arc::new(move || {
            let _ = tx.send(());
        }));
        rx
    }

    fn wait(rx: &crossbeam_channel::Receiver<()>) {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("a new port list");
    }

    fn ports(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn nothing_is_listed_before_the_first_read() {
        let list = PortList::new("serial ports", Arc::new(|| ports(&["COM3"])));
        assert_eq!(list.newer_than(0), None);
        assert!(!list.is_reading());
    }

    #[test]
    fn a_read_runs_on_a_helper_thread_and_is_sorted() {
        let (thread_tx, thread_rx) = crossbeam_channel::bounded(1);
        let list = PortList::new(
            "serial ports",
            Arc::new(move || {
                thread_tx.send(std::thread::current().id()).unwrap();
                ports(&["COM4", "COM3"])
            }),
        );
        let rx = notifications(&list);

        list.refresh();
        wait(&rx);

        assert_ne!(
            thread_rx.recv().unwrap(),
            std::thread::current().id(),
            "the caller's thread never lists ports"
        );
        assert_eq!(list.newer_than(0), Some((1, ports(&["COM3", "COM4"]))));
        assert_eq!(list.newer_than(1), None, "each list is handed back once");
        assert!(!list.is_reading());
    }

    #[test]
    fn refreshes_during_a_read_become_one_more_read() {
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded::<()>();
        let reads = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&reads);
        let list = PortList::new(
            "serial ports",
            Arc::new(move || {
                let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                ports(&[if n == 1 { "COM3" } else { "COM4" }])
            }),
        );
        let rx = notifications(&list);

        list.refresh();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..3 {
            list.refresh();
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1, "one read at a time");
        assert!(list.is_reading());

        release_tx.send(()).unwrap();
        wait(&rx);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        release_tx.send(()).unwrap();
        wait(&rx);

        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "three held refreshes are answered by one more read"
        );
        assert_eq!(list.newer_than(1), Some((2, ports(&["COM4"]))));
        assert!(!list.is_reading());
    }

    #[test]
    fn a_wedged_read_keeps_the_last_list() {
        let (release_tx, release_rx) = crossbeam_channel::unbounded::<()>();
        let reads = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&reads);
        let list = PortList::new(
            "serial ports",
            Arc::new(move || {
                if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                    // The driver holds every read after the first.
                    release_rx.recv().unwrap();
                }
                ports(&["COM3"])
            }),
        );
        let rx = notifications(&list);

        list.refresh();
        wait(&rx);
        list.refresh();
        list.refresh();

        assert!(list.is_reading());
        assert_eq!(
            list.newer_than(0),
            Some((1, ports(&["COM3"]))),
            "the last list stays while a read is held"
        );

        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        wait(&rx);
        wait(&rx);
        assert!(!list.is_reading());
    }

    fn address(ip: [u8; 4], interface: &str) -> LocalAddress {
        LocalAddress {
            ip: Ipv4Addr::from(ip),
            interface: interface.to_owned(),
        }
    }

    #[test]
    fn addresses_are_in_order_with_loopback_last_and_each_once() {
        let ordered = one_per_address(vec![
            address([127, 0, 0, 1], "Loopback Pseudo-Interface 1"),
            address([192, 168, 1, 20], "Ethernet"),
            address([10, 0, 0, 5], "Wi-Fi"),
            address([192, 168, 1, 20], "Ethernet 2"),
        ]);
        assert_eq!(
            ordered,
            [
                address([10, 0, 0, 5], "Wi-Fi"),
                address([192, 168, 1, 20], "Ethernet"),
                address([127, 0, 0, 1], "Loopback Pseudo-Interface 1"),
            ]
        );
    }

    #[test]
    fn this_computer_lists_its_loopback_address_last() {
        // Asks the real system: every host this runs on has loopback up.
        let listed = enumerate_local_addresses();
        assert!(
            listed.last().is_some_and(|a| a.ip == Ipv4Addr::LOCALHOST),
            "{listed:?}"
        );
    }
}
