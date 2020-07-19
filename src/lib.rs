//! Redirect traffic to a SOCKS proxy with pcap.

use log::{debug, info, trace, warn};
use lru::LruCache;
use rand::{self, Rng};
use std::cmp::{max, min};
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{self, Duration, Instant, SystemTime};
use tokio::io;

pub mod cache;
pub mod packet;
pub mod pcap;
pub mod socks;

use self::socks::{DatagramWorker, ForwardDatagram, ForwardStream, StreamWorker};
use cache::{Queue, Window};
use packet::layer::arp::Arp;
use packet::layer::ethernet::Ethernet;
use packet::layer::ipv4::Ipv4;
use packet::layer::tcp::Tcp;
use packet::layer::udp::Udp;
use packet::layer::{Layer, LayerKind, LayerKinds, Layers};
use packet::{Defraggler, Indicator};
use pcap::Interface;
use pcap::{HardwareAddr, Receiver, Sender};

/// Gets a list of available network interfaces for the current machine.
pub fn interfaces() -> Vec<Interface> {
    pcap::interfaces()
        .into_iter()
        .filter(|inter| !inter.is_loopback)
        .collect()
}

/// Gets a list of available network interfaces which is possibly can be used for the current machine.
fn auto_interfaces() -> Vec<Interface> {
    // With specified IP address
    let mut inters: Vec<Interface> = interfaces()
        .into_iter()
        .filter(|inter| !inter.ip_addrs[0].is_unspecified())
        .collect();
    // Is up
    if inters.len() > 1 {
        inters = inters.into_iter().filter(|inter| inter.is_up).collect();
    }

    inters
}

/// Gets an available network interface.
pub fn interface(name: Option<String>) -> Option<Interface> {
    let inters = match name {
        Some(ref name) => {
            let mut inters = interfaces();
            inters.retain(|ref current_inter| &current_inter.name == name);
            inters
        }
        None => auto_interfaces(),
    };

    if inters.len() != 1 {
        None
    } else {
        Some(inters[0].clone())
    }
}

/// Represents the wait time after a `TimedOut` `IoError`.
const TIMEDOUT_WAIT: u64 = 20;

/// Represents the max distance of `u32` values between packets in an `u32` window.
const MAX_U32_WINDOW_SIZE: usize = 16 * 1024 * 1024;

/// Represents if the TCP timestamp option is enabled.
const ENABLE_TIMESTAMP: bool = false;
/// Represents the frequency of the update of the timestamp. The
/// [RFC 7323](https://tools.ietf.org/html/rfc7323) describes the timestamp clock
/// must not be "too fast". And a reasonable value is 1 ms to 1 sec per tick.
/// A `TIMESTAMP_RATE` 1 represents 1 ms and 1000 represents 1 sec per tick.
const TIMESTAMP_RATE: u128 = 1;

/// Represents if the received send MSS should be preferred instead of manually set MTU in TCP.
const PREFER_SEND_MSS: bool = true;

/// Represents the timeout for a retransmission in a TCP connection.
const RTO: u64 = 3000;

/// Represents the minimum packet size.
/// Because all traffic is in Ethernet, and the 802.3 specifies the minimum is 64 Bytes.
/// Exclude the 4 bytes used in FCS, the minimum packet size in pcap2socks is 60 Bytes.
const MINIMUM_PACKET_SIZE: usize = 60;

/// Represents a channel forward traffic to the source in pcap.
pub struct Forwarder {
    tx: Sender,
    mtu: u16,
    src_hardware_addr: HardwareAddr,
    local_hardware_addr: HardwareAddr,
    src_ip_addr: Ipv4Addr,
    local_ip_addr: Ipv4Addr,
    ipv4_identification_map: HashMap<Ipv4Addr, u16>,
    tcp_fin_map: HashMap<(u16, SocketAddrV4), Instant>,
    tcp_send_window_map: HashMap<(u16, SocketAddrV4), usize>,
    tcp_send_mss_map: HashMap<(u16, SocketAddrV4), u16>,
    tcp_sequence_map: HashMap<(u16, SocketAddrV4), u32>,
    tcp_acknowledgement_map: HashMap<(u16, SocketAddrV4), u32>,
    tcp_window_map: HashMap<(u16, SocketAddrV4), u16>,
    tcp_wscale_map: HashMap<(u16, SocketAddrV4), u8>,
    tcp_sacks_map: HashMap<(u16, SocketAddrV4), Vec<(u32, u32)>>,
    tcp_ts_map: HashMap<(u16, SocketAddrV4), u32>,
    tcp_cache_map: HashMap<(u16, SocketAddrV4), Queue>,
    tcp_queue_map: HashMap<(u16, SocketAddrV4), VecDeque<u8>>,
}

impl Forwarder {
    /// Creates a new `Forwarder`.
    pub fn new(
        tx: Sender,
        mtu: u16,
        local_hardware_addr: HardwareAddr,
        src_ip_addr: Ipv4Addr,
        local_ip_addr: Ipv4Addr,
    ) -> Forwarder {
        Forwarder {
            tx,
            mtu,
            src_hardware_addr: pcap::HARDWARE_ADDR_UNSPECIFIED,
            local_hardware_addr,
            src_ip_addr,
            local_ip_addr,
            ipv4_identification_map: HashMap::new(),
            tcp_fin_map: HashMap::new(),
            tcp_send_window_map: HashMap::new(),
            tcp_send_mss_map: HashMap::new(),
            tcp_sequence_map: HashMap::new(),
            tcp_acknowledgement_map: HashMap::new(),
            tcp_window_map: HashMap::new(),
            tcp_wscale_map: HashMap::new(),
            tcp_sacks_map: HashMap::new(),
            tcp_ts_map: HashMap::new(),
            tcp_cache_map: HashMap::new(),
            tcp_queue_map: HashMap::new(),
        }
    }

    /// Sets the source hardware address.
    pub fn set_src_hardware_addr(&mut self, hardware_addr: HardwareAddr) {
        self.src_hardware_addr = hardware_addr;
        trace!("set source hardware address to {}", hardware_addr);
    }

    /// Sets the local IP address.
    pub fn set_local_ip_addr(&mut self, ip_addr: Ipv4Addr) {
        self.local_ip_addr = ip_addr;
        trace!("set local IP address to {}", ip_addr);
    }

    fn increase_ipv4_identification(&mut self, ip_addr: Ipv4Addr) {
        let entry = self.ipv4_identification_map.entry(ip_addr).or_insert(0);
        *entry = entry.checked_add(1).unwrap_or(0);
        trace!("increase IPv4 identification of {} to {}", ip_addr, entry);
    }

    /// Sets the send window size of a TCP connection.
    pub fn set_tcp_send_window(&mut self, dst: SocketAddrV4, src_port: u16, window: usize) {
        self.tcp_send_window_map.insert((src_port, dst), window);
        trace!(
            "set TCP send window of {} -> {} to {}",
            src_port,
            dst,
            window,
        );
    }

    /// Sets the send MSS of a TCP connection.
    pub fn set_tcp_send_mss(&mut self, dst: SocketAddrV4, src_port: u16, mss: u16) {
        self.tcp_send_mss_map.insert((src_port, dst), mss);
        trace!("set TCP send MSS of {} -> {} to {}", src_port, dst, mss);
    }

    /// Sets the sequence of a TCP connection. In fact, this function should never be used.
    pub fn set_tcp_sequence(&mut self, dst: SocketAddrV4, src_port: u16, acknowledgement: u32) {
        self.tcp_sequence_map
            .insert((src_port, dst), acknowledgement);
        trace!(
            "set TCP sequence of {} -> {} to {}",
            dst,
            src_port,
            acknowledgement
        );
    }

    /// Sets the acknowledgement of a TCP connection.
    pub fn set_tcp_acknowledgement(&mut self, dst: SocketAddrV4, src_port: u16, sequence: u32) {
        self.tcp_acknowledgement_map
            .insert((src_port, dst), sequence);
        trace!(
            "set TCP acknowledgement of {} -> {} to {}",
            dst,
            src_port,
            sequence
        );
    }

    /// Adds acknowledgement to a TCP connection.
    pub fn add_tcp_acknowledgement(&mut self, dst: SocketAddrV4, src_port: u16, n: u32) {
        let entry = self
            .tcp_acknowledgement_map
            .entry((src_port, dst))
            .or_insert(0);
        *entry = entry
            .checked_add(n)
            .unwrap_or_else(|| n - (u32::MAX - *entry));
        trace!(
            "add TCP acknowledgement of {} -> {} to {}",
            dst,
            src_port,
            entry
        );
    }

    /// Sets the window size of a TCP connection.
    pub fn set_tcp_window(&mut self, dst: SocketAddrV4, src_port: u16, window: u16) {
        self.tcp_window_map.insert((src_port, dst), window);
        trace!("set TCP window of {} -> {} to {}", dst, src_port, window);
    }

    /// Sets the window scale of a TCP connection.
    pub fn set_tcp_wscale(&mut self, dst: SocketAddrV4, src_port: u16, wscale: u8) {
        self.tcp_wscale_map.insert((src_port, dst), wscale);
        trace!("set TCP wscale of {} -> {} to {}", dst, src_port, wscale);
    }

    /// Sets the selective acknowledgements of a TCP connection.
    pub fn set_tcp_sacks(&mut self, dst: SocketAddrV4, src_port: u16, sacks: &Vec<(u32, u32)>) {
        if sacks.len() <= 0 {
            self.tcp_sacks_map.remove(&(src_port, dst));
            trace!("remove TCP sack of {} -> {}", dst, src_port);
        } else {
            let size = min(4, sacks.len());
            self.tcp_sacks_map
                .insert((src_port, dst), Vec::from(&sacks[..size]));
            let mut desc = format!("[{}, {}]", sacks[0].0, sacks[0].1);
            if sacks.len() > 1 {
                desc += format!(" and {} more", sacks.len() - 1).as_str();
            }
            trace!("set TCP sack of {} -> {} to {}", dst, src_port, desc);
        }
    }

    /// Sets the timestamp of a TCP connection, this will enable the timestamp in TCP traffic.
    pub fn set_tcp_ts(&mut self, dst: SocketAddrV4, src_port: u16, ts: u32) {
        self.tcp_ts_map.insert((src_port, dst), ts);
        trace!("set TCP timestamp of {} -> {} to {}", dst, src_port, ts);
    }

    fn generate_ts(&self, dst: SocketAddrV4, src_port: u16) -> Option<(u32, u32)> {
        let key = (src_port, dst);

        if ENABLE_TIMESTAMP {
            match self.tcp_ts_map.get(&key) {
                Some(&ts) => {
                    let now = SystemTime::now().duration_since(time::UNIX_EPOCH);

                    match now {
                        Ok(now) => {
                            let now = now.as_millis() / TIMESTAMP_RATE;

                            Some((now as u32, ts))
                        }
                        Err(_) => None,
                    }
                }
                None => None,
            }
        } else {
            None
        }
    }

    /// Invalidates TCP cache to the given sequence.
    pub fn invalidate_cache_to(&mut self, dst: SocketAddrV4, src_port: u16, sequence: u32) {
        if let Some(cache) = self.tcp_cache_map.get_mut(&(src_port, dst)) {
            cache.invalidate_to(sequence);
        }
        trace!(
            "invalidate cache {} -> {} to sequence {}",
            dst,
            src_port,
            sequence
        );
    }

    /// Removes all information related to a TCP connection.
    pub fn remove(&mut self, dst: SocketAddrV4, src_port: u16) {
        let key = (src_port, dst);

        self.tcp_fin_map.remove(&key);
        self.tcp_send_window_map.remove(&key);
        self.tcp_send_mss_map.remove(&key);
        self.tcp_sequence_map.remove(&key);
        self.tcp_acknowledgement_map.remove(&key);
        self.tcp_window_map.remove(&key);
        self.tcp_wscale_map.remove(&key);
        self.tcp_sacks_map.remove(&key);
        self.tcp_ts_map.remove(&key);
        self.tcp_cache_map.remove(&key);
        if let Some(cache) = self.tcp_cache_map.get(&key) {
            if !cache.is_empty() {
                trace!(
                    "cache {} -> {} was removed while the cache is not empty",
                    dst,
                    src_port
                );
            }
        }
        self.tcp_queue_map.remove(&key);
        if let Some(queue) = self.tcp_queue_map.get(&key) {
            if !queue.is_empty() {
                trace!(
                    "queue {} -> {} was removed while the queue is not empty",
                    dst,
                    src_port
                );
            }
        }
        trace!("remove {} -> {}", dst, src_port);
    }

    /// Get the size of the cache and the queue of a TCP connection.
    pub fn get_cache_size(&mut self, dst: SocketAddrV4, src_port: u16) -> usize {
        let key = (src_port, dst);

        let mut size = 0;
        if let Some(cache) = self.tcp_cache_map.get(&key) {
            size += cache.len();
        }
        if let Some(queue) = self.tcp_queue_map.get(&key) {
            size += queue.len();
        }

        size
    }

    /// Sends an ARP reply packet.
    pub fn send_arp_reply(&mut self) -> io::Result<()> {
        // ARP
        let arp = Arp::new_reply(
            self.local_hardware_addr,
            self.local_ip_addr,
            self.src_hardware_addr,
            self.src_ip_addr,
        );

        // Ethernet
        let ethernet =
            Ethernet::new(arp.kind(), arp.src_hardware_addr(), arp.dst_hardware_addr()).unwrap();

        // Indicator
        let indicator = Indicator::new(Layers::Ethernet(ethernet), Some(Layers::Arp(arp)), None);

        // Send
        self.send(&indicator)
    }

    /// Appends TCP ACK payload to queue.
    pub fn append_to_queue(
        &mut self,
        dst: SocketAddrV4,
        src_port: u16,
        payload: &[u8],
    ) -> io::Result<()> {
        let key = (src_port, dst);

        // Append to queue
        let queue = self
            .tcp_queue_map
            .entry(key)
            .or_insert_with(|| VecDeque::new());
        queue.extend(payload);
        trace!(
            "append {} Bytes to TCP queue {} -> {}",
            payload.len(),
            dst,
            src_port
        );

        self.send_tcp_ack(dst, src_port)
    }

    /// Retransmits TCP ACK packets from the cache. This method is used for fast retransmission.
    pub fn retransmit_tcp_ack(&mut self, dst: SocketAddrV4, src_port: u16) -> io::Result<()> {
        let key = (src_port, dst);

        // Retransmit
        let payload;
        let sequence;
        match self.tcp_cache_map.get(&key) {
            Some(cache) => {
                payload = cache.get_all();
                sequence = cache.sequence();
            }
            None => return Ok(()),
        };

        if payload.len() > 0 {
            trace!(
                "retransmit {} Bytes {} -> {} from {}",
                payload.len(),
                dst,
                src_port,
                sequence
            );
            self.send_tcp_ack_raw(dst, src_port, sequence, payload.as_slice())?;
        }

        Ok(())
    }

    /// Retransmits TCP ACK packets from the cache excluding the certain edges. This method is used
    /// for fast retransmission.
    pub fn retransmit_tcp_ack_without(
        &mut self,
        dst: SocketAddrV4,
        src_port: u16,
        sacks: Vec<(u32, u32)>,
    ) -> io::Result<()> {
        let key = (src_port, dst);

        if let None = self.tcp_cache_map.get(&key) {
            return Err(io::Error::new(io::ErrorKind::Other, "cannot get cache"));
        }

        let mut sequence = self.tcp_cache_map.get(&key).unwrap().sequence();
        let mut size = self.tcp_cache_map.get(&key).unwrap().len();
        let recv_next = sequence
            .checked_add(size as u32)
            .unwrap_or_else(|| size as u32 - (u32::MAX - sequence));

        // Find all disjointed ranges
        let mut ranges = Vec::new();
        ranges.push((sequence, recv_next));
        for sack in sacks {
            let mut temp_ranges = Vec::new();

            for range in ranges {
                for temp_range in disjoint_u32_range(range, sack) {
                    temp_ranges.push(temp_range);
                }
            }

            ranges = temp_ranges;
        }

        // Update the last range
        if let Some(range) = ranges.last() {
            let last_recv_next = range.1;
            size = recv_next
                .checked_sub(last_recv_next)
                .unwrap_or_else(|| sequence + (u32::MAX - last_recv_next))
                as usize;
            sequence = last_recv_next;
        }

        // Retransmit
        for range in ranges {
            let size = range
                .1
                .checked_sub(range.0)
                .unwrap_or_else(|| range.1 + (u32::MAX - range.0)) as usize;
            let payload = self.tcp_cache_map.get(&key).unwrap().get(range.0, size)?;
            if payload.len() > 0 {
                self.send_tcp_ack_raw(dst, src_port, range.0, payload.as_slice())?;
            }
        }
        let payload = self.tcp_cache_map.get(&key).unwrap().get(sequence, size)?;
        if payload.len() > 0 {
            trace!(
                "retransmit {} Bytes {} -> {} from {}",
                payload.len(),
                dst,
                src_port,
                sequence
            );
            self.send_tcp_ack_raw(dst, src_port, sequence, payload.as_slice())?;
        }

        Ok(())
    }

    /// Retransmits timed out TCP ACK packets from the cache. This method is used for transmitting
    /// timed out data.
    pub fn retransmit_tcp_ack_timedout(
        &mut self,
        dst: SocketAddrV4,
        src_port: u16,
    ) -> io::Result<()> {
        let key = (src_port, dst);

        // Retransmit
        let mut payload = Vec::new();
        let mut sequence = 0;
        if let Some(cache) = self.tcp_cache_map.get(&key) {
            payload = cache.get_timed_out(Duration::from_millis(RTO));
            sequence = cache.sequence();
        };

        if payload.len() > 0 {
            trace!(
                "retransmit {} Bytes {} -> {} from {} due to timeout",
                payload.len(),
                dst,
                src_port,
                sequence
            );
            self.send_tcp_ack_raw(dst, src_port, sequence, payload.as_slice())?;
        } else {
            // FIN
            if self.tcp_fin_map.contains_key(&key) {
                let is_queue_empty = match self.tcp_queue_map.get(&key) {
                    Some(cache) => cache.is_empty(),
                    None => true,
                };
                let is_cache_empty = match self.tcp_cache_map.get(&key) {
                    Some(cache) => cache.is_empty(),
                    None => true,
                };
                if is_queue_empty && is_cache_empty {
                    trace!("retransmit TCP FIN {} -> {} due to timeout", dst, src_port,);
                    // Send
                    self.send_tcp_fin(dst, src_port)?;
                }
            }
        }

        Ok(())
    }

    /// Sends TCP ACK packets from the queue.
    pub fn send_tcp_ack(&mut self, dst: SocketAddrV4, src_port: u16) -> io::Result<()> {
        let key = (src_port, dst);

        if let Some(queue) = self.tcp_queue_map.get_mut(&key) {
            let window = *self.tcp_send_window_map.get(&key).unwrap_or(&0);
            if window > 0 {
                // TCP sequence
                let sequence = *self.tcp_sequence_map.get(&key).unwrap_or(&0);
                let wscale = *self.tcp_wscale_map.get(&key).unwrap_or(&0);

                let cache = self.tcp_cache_map.entry(key).or_insert_with(|| {
                    Queue::with_capacity((u16::MAX as usize) << wscale as usize, sequence)
                });
                let sent_size = cache.len();
                let remain_size = window.checked_sub(sent_size).unwrap_or(0);
                let remain_size = min(remain_size, u16::MAX as usize) as u16;

                let size = min(remain_size as usize, queue.len());
                if size > 0 {
                    let payload: Vec<u8> = queue.drain(..size).collect();

                    // Append to cache
                    let cache = self.tcp_cache_map.entry(key).or_insert_with(|| {
                        Queue::with_capacity((u16::MAX as usize) << wscale as usize, sequence)
                    });
                    cache.append(&payload)?;

                    // Send
                    self.send_tcp_ack_raw(dst, src_port, sequence, &payload)?;
                }
            }
        }

        // FIN
        if self.tcp_fin_map.contains_key(&key) {
            let is_queue_empty = match self.tcp_queue_map.get(&key) {
                Some(cache) => cache.is_empty(),
                None => true,
            };
            let is_cache_empty = match self.tcp_cache_map.get(&key) {
                Some(cache) => cache.is_empty(),
                None => true,
            };
            if is_queue_empty && is_cache_empty {
                // Send
                self.send_tcp_fin(dst, src_port)?;
            }
        }

        Ok(())
    }

    fn send_tcp_ack_raw(
        &mut self,
        dst: SocketAddrV4,
        src_port: u16,
        sequence: u32,
        payload: &[u8],
    ) -> io::Result<()> {
        let key = (src_port, dst);

        // Pseudo headers
        let tcp = Tcp::new_ack(
            0,
            0,
            0,
            0,
            0,
            self.generate_ts(dst, src_port),
            self.tcp_sacks_map.get(&key),
        );
        let ipv4 = Ipv4::new(0, tcp.kind(), Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED).unwrap();

        // Segmentation
        let header_size = ipv4.len() + tcp.len();
        let max_payload_size = match PREFER_SEND_MSS {
            true => match self.tcp_send_mss_map.get(&key) {
                Some(&mss) => mss as usize,
                None => self.mtu as usize - header_size,
            },
            false => self.mtu as usize - header_size,
        };
        let mut i = 0;
        while max_payload_size * i < payload.len() {
            let length = min(max_payload_size, payload.len() - i * max_payload_size);
            let payload = &payload[i * max_payload_size..i * max_payload_size + length];
            let sequence = sequence
                .checked_add((i * max_payload_size) as u32)
                .unwrap_or_else(|| (i * max_payload_size) as u32 - (u32::MAX - sequence));

            // TCP
            let tcp = Tcp::new_ack(
                dst.port(),
                src_port,
                sequence,
                *self.tcp_acknowledgement_map.get(&key).unwrap_or(&0),
                *self.tcp_window_map.get(&key).unwrap_or(&65535),
                self.generate_ts(dst, src_port),
                self.tcp_sacks_map.get(&key),
            );

            // Send
            self.send_ipv4_with_transport(dst.ip().clone(), Layers::Tcp(tcp), Some(payload))?;

            // Update TCP sequence
            let next_sequence = sequence
                .checked_add(length as u32)
                .unwrap_or_else(|| length as u32 - (u32::MAX - sequence));
            let record_sequence = *self.tcp_sequence_map.get(&key).unwrap_or(&0);
            let sub_sequence = next_sequence
                .checked_sub(record_sequence)
                .unwrap_or_else(|| next_sequence + (u32::MAX - record_sequence));
            if (sub_sequence as usize) <= MAX_U32_WINDOW_SIZE {
                self.tcp_sequence_map.insert(key, next_sequence);
            }

            i = i + 1;
        }

        Ok(())
    }

    /// Sends an TCP ACK packet without payload.
    pub fn send_tcp_ack_0(&mut self, dst: SocketAddrV4, src_port: u16) -> io::Result<()> {
        let key = (src_port, dst);

        // TCP
        let tcp = Tcp::new_ack(
            dst.port(),
            src_port,
            *self.tcp_sequence_map.get(&key).unwrap_or(&0),
            *self.tcp_acknowledgement_map.get(&key).unwrap_or(&0),
            *self.tcp_window_map.get(&key).unwrap_or(&65535),
            self.generate_ts(dst, src_port),
            self.tcp_sacks_map.get(&key),
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), Layers::Tcp(tcp), None)
    }

    /// Sends an TCP ACK/SYN packet.
    pub fn send_tcp_ack_syn(
        &mut self,
        dst: SocketAddrV4,
        src_port: u16,
        mss: Option<u16>,
        wscale: Option<u8>,
        sack_perm: bool,
    ) -> io::Result<()> {
        let key = (src_port, dst);

        // TCP
        let tcp = Tcp::new_ack_syn(
            dst.port(),
            src_port,
            *self.tcp_sequence_map.get(&key).unwrap_or(&0),
            *self.tcp_acknowledgement_map.get(&key).unwrap_or(&0),
            *self.tcp_window_map.get(&key).unwrap_or(&65535),
            mss,
            wscale,
            sack_perm,
            self.generate_ts(dst, src_port),
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), Layers::Tcp(tcp), None)?;

        // Update TCP sequence
        let tcp_sequence_entry = self.tcp_sequence_map.entry(key).or_insert(0);
        *tcp_sequence_entry = tcp_sequence_entry.checked_add(1).unwrap_or(0);

        Ok(())
    }

    /// Sends an TCP ACK/RST packet.
    pub fn send_tcp_ack_rst(&mut self, dst: SocketAddrV4, src_port: u16) -> io::Result<()> {
        let key = (src_port, dst);

        // TCP
        let tcp = Tcp::new_ack_rst(
            dst.port(),
            src_port,
            *self.tcp_sequence_map.get(&key).unwrap_or(&0),
            *self.tcp_acknowledgement_map.get(&key).unwrap_or(&0),
            *self.tcp_window_map.get(&key).unwrap_or(&65535),
            self.generate_ts(dst, src_port),
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), Layers::Tcp(tcp), None)
    }

    /// Sends an TCP ACK/FIN packet.
    pub fn send_tcp_ack_fin(&mut self, dst: SocketAddrV4, src_port: u16) -> io::Result<()> {
        let key = (src_port, dst);

        // TCP
        let tcp = Tcp::new_ack_fin(
            dst.port(),
            src_port,
            *self.tcp_sequence_map.get(&key).unwrap_or(&0),
            *self.tcp_acknowledgement_map.get(&key).unwrap_or(&0),
            *self.tcp_window_map.get(&key).unwrap_or(&65535),
            self.generate_ts(dst, src_port),
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), Layers::Tcp(tcp), None)
    }

    /// Sends an TCP RST packet.
    pub fn send_tcp_rst(
        &mut self,
        dst: SocketAddrV4,
        src_port: u16,
        ts: Option<u32>,
    ) -> io::Result<()> {
        let key = (src_port, dst);

        // TCP
        let ts = match ts {
            Some(ts) => Some((ts, 0)),
            None => None,
        };
        let tcp = Tcp::new_rst(
            dst.port(),
            src_port,
            *self.tcp_sequence_map.get(&key).unwrap_or(&0),
            0,
            *self.tcp_window_map.get(&key).unwrap_or(&65535),
            ts,
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), Layers::Tcp(tcp), None)
    }

    fn send_tcp_fin(&mut self, dst: SocketAddrV4, src_port: u16) -> io::Result<()> {
        let key = (src_port, dst);

        // TCP
        let tcp = Tcp::new_fin(
            dst.port(),
            src_port,
            *self.tcp_sequence_map.get(&key).unwrap_or(&0),
            *self.tcp_acknowledgement_map.get(&key).unwrap_or(&0),
            *self.tcp_window_map.get(&key).unwrap_or(&65535),
            self.generate_ts(dst, src_port),
        );

        // Send
        self.send_ipv4_with_transport(dst.ip().clone(), Layers::Tcp(tcp), None)
    }

    /// Sends UDP packets.
    pub fn send_udp(&mut self, dst: SocketAddrV4, src_port: u16, payload: &[u8]) -> io::Result<()> {
        // Pseudo headers
        let udp = Udp::new(0, 0);
        let ipv4 = Ipv4::new(0, udp.kind(), Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED).unwrap();

        // Fragmentation
        let ipv4_header_size = ipv4.len();
        let udp_header_size = udp.len();

        let size = udp_header_size + payload.len();
        let mut n = 0;
        while n < size {
            let mut length = min(size - n, self.mtu as usize - ipv4_header_size);
            let mut remain = size - n - length;

            // Alignment
            if remain > 0 {
                length = length / 8 * 8;
                remain = size - n - length;
            }

            // Leave at least 8 Bytes for last fragment
            if remain > 0 && remain < 8 {
                length = length - 8;
            }

            // Send
            if n == 0 {
                if remain > 0 {
                    // UDP
                    let udp = Udp::new(dst.port(), src_port);

                    self.send_ipv4_more_fragment(
                        dst.ip().clone(),
                        udp.kind(),
                        (n / 8) as u16,
                        Some(Layers::Udp(udp)),
                        &payload[..length - udp_header_size],
                    )?;
                } else {
                    self.send_udp_raw(dst, src_port, payload)?;
                }
            } else {
                if remain > 0 {
                    self.send_ipv4_more_fragment(
                        dst.ip().clone(),
                        udp.kind(),
                        (n / 8) as u16,
                        None,
                        &payload[n - udp_header_size..n + length - udp_header_size],
                    )?;
                } else {
                    self.send_ipv4_last_fragment(
                        dst.ip().clone(),
                        udp.kind(),
                        (n / 8) as u16,
                        &payload[n - udp_header_size..n + length - udp_header_size],
                    )?;
                }
            }

            n = n + length;
        }

        Ok(())
    }

    fn send_udp_raw(&mut self, dst: SocketAddrV4, src_port: u16, payload: &[u8]) -> io::Result<()> {
        // UDP
        let udp = Udp::new(dst.port(), src_port);

        self.send_ipv4_with_transport(dst.ip().clone(), Layers::Udp(udp), Some(payload))
    }

    fn send_ipv4_more_fragment(
        &mut self,
        dst_ip_addr: Ipv4Addr,
        t: LayerKind,
        fragment_offset: u16,
        mut transport: Option<Layers>,
        payload: &[u8],
    ) -> io::Result<()> {
        // IPv4
        let ipv4 = Ipv4::new_more_fragment(
            *self.ipv4_identification_map.get(&dst_ip_addr).unwrap_or(&0),
            t,
            fragment_offset,
            dst_ip_addr,
            self.src_ip_addr,
        )
        .unwrap();

        // Set IPv4 layer for checksum
        if let Some(ref mut transport) = transport {
            match transport {
                Layers::Tcp(ref mut tcp) => tcp.set_ipv4_layer(&ipv4),
                Layers::Udp(ref mut udp) => udp.set_ipv4_layer(&ipv4),
                _ => {}
            }
        };

        // Send
        self.send_ethernet(Layers::Ipv4(ipv4), transport, Some(payload))
    }

    fn send_ipv4_last_fragment(
        &mut self,
        dst_ip_addr: Ipv4Addr,
        t: LayerKind,
        fragment_offset: u16,
        payload: &[u8],
    ) -> io::Result<()> {
        // IPv4
        let ipv4 = Ipv4::new_last_fragment(
            *self.ipv4_identification_map.get(&dst_ip_addr).unwrap_or(&0),
            t,
            fragment_offset,
            dst_ip_addr,
            self.src_ip_addr,
        )
        .unwrap();

        // Send
        self.send_ethernet(Layers::Ipv4(ipv4), None, Some(payload))?;

        // Update IPv4 identification
        self.increase_ipv4_identification(dst_ip_addr);

        Ok(())
    }

    fn send_ipv4_with_transport(
        &mut self,
        dst_ip_addr: Ipv4Addr,
        mut transport: Layers,
        payload: Option<&[u8]>,
    ) -> io::Result<()> {
        // IPv4
        let ipv4 = Ipv4::new(
            *self.ipv4_identification_map.get(&dst_ip_addr).unwrap_or(&0),
            transport.kind(),
            dst_ip_addr,
            self.src_ip_addr,
        )
        .unwrap();

        // Set IPv4 layer for checksum
        match transport {
            Layers::Tcp(ref mut tcp) => tcp.set_ipv4_layer(&ipv4),
            Layers::Udp(ref mut udp) => udp.set_ipv4_layer(&ipv4),
            _ => {}
        }

        // Send
        self.send_ethernet(Layers::Ipv4(ipv4), Some(transport), payload)?;

        // Update IPv4 identification
        self.increase_ipv4_identification(dst_ip_addr);

        Ok(())
    }

    fn send_ethernet(
        &mut self,
        network: Layers,
        transport: Option<Layers>,
        payload: Option<&[u8]>,
    ) -> io::Result<()> {
        // Ethernet
        let ethernet = Ethernet::new(
            network.kind(),
            self.local_hardware_addr,
            self.src_hardware_addr,
        )
        .unwrap();

        // Indicator
        let indicator = Indicator::new(Layers::Ethernet(ethernet), Some(network), transport);

        // Send
        match payload {
            Some(payload) => self.send_with_payload(&indicator, payload),
            None => self.send(&indicator),
        }
    }

    fn send(&mut self, indicator: &Indicator) -> io::Result<()> {
        // Serialize
        let size = indicator.len();
        let buffer_size = max(size, MINIMUM_PACKET_SIZE);
        let mut buffer = vec![0u8; buffer_size];
        indicator.serialize(&mut buffer[..size])?;

        // Send
        self.tx.send_to(&buffer, None).unwrap_or(Ok(()))?;
        debug!("send to pcap: {} ({} Bytes)", indicator.brief(), size);

        Ok(())
    }

    fn send_with_payload(&mut self, indicator: &Indicator, payload: &[u8]) -> io::Result<()> {
        // Serialize
        let size = indicator.len();
        let buffer_size = max(size + payload.len(), MINIMUM_PACKET_SIZE);
        let mut buffer = vec![0u8; buffer_size];
        indicator.serialize_with_payload(&mut buffer[..size + payload.len()], payload)?;

        // Send
        self.tx.send_to(&buffer, None).unwrap_or(Ok(()))?;
        debug!(
            "send to pcap: {} ({} + {} Bytes)",
            indicator.brief(),
            size,
            payload.len()
        );

        Ok(())
    }
}

impl ForwardStream for Forwarder {
    fn forward(&mut self, dst: SocketAddrV4, src_port: u16, payload: &[u8]) -> io::Result<()> {
        self.append_to_queue(dst, src_port, payload)
    }

    fn tick(&mut self, dst: SocketAddrV4, src_port: u16) -> io::Result<()> {
        self.retransmit_tcp_ack_timedout(dst, src_port)
    }

    fn close(&mut self, dst: SocketAddrV4, src_port: u16) -> io::Result<()> {
        let key = (src_port, dst);

        self.tcp_fin_map.insert(key, Instant::now());

        self.send_tcp_ack(dst, src_port)
    }
}

impl ForwardDatagram for Forwarder {
    fn forward(&mut self, dst: SocketAddrV4, src_port: u16, payload: &[u8]) -> io::Result<()> {
        self.send_udp(dst, src_port, payload)
    }
}

fn disjoint_u32_range(main: (u32, u32), sub: (u32, u32)) -> Vec<(u32, u32)> {
    let size_main = main
        .1
        .checked_sub(main.0)
        .unwrap_or_else(|| main.1 + (u32::MAX - main.0)) as usize;
    let diff_first = sub
        .0
        .checked_sub(main.0)
        .unwrap_or_else(|| sub.0 + (u32::MAX - main.0)) as usize;
    let diff_second = sub
        .1
        .checked_sub(main.1)
        .unwrap_or_else(|| sub.1 + (u32::MAX - main.1)) as usize;
    let mut vector = Vec::with_capacity(2);

    if diff_first <= MAX_U32_WINDOW_SIZE {
        if diff_second > MAX_U32_WINDOW_SIZE {
            // sub is in the main
            vector.push((main.0, sub.0));
            vector.push((sub.1, main.1));
        } else {
            if diff_first >= size_main {
                // sub is in the right of the main
                vector.push((main.0, main.1));
            } else {
                // sub overlaps the right part of the main
                vector.push((main.0, sub.0));
            }
        }
    } else {
        if diff_second > MAX_U32_WINDOW_SIZE {
            // The distance between the main's left edge and the sub's right edge
            let diff = sub
                .1
                .checked_sub(main.0)
                .unwrap_or_else(|| sub.1 + (u32::MAX - main.0)) as usize;
            if diff > MAX_U32_WINDOW_SIZE {
                // sub is in the left of the main
                vector.push((main.0, main.1));
            } else {
                // sub overlaps the left part of the main
                vector.push((sub.1, main.1));
            }
        } else {
            // sub covers the main
        }
    }

    vector
}

/// Represents the TCP ACK duplicates before trigger a fast retransmission.
const DUPLICATES_BEFORE_FAST_RETRANSMISSION: usize = 3;
/// Represents the cool down time between 2 retransmissions.
const RETRANSMISSION_COOL_DOWN: u128 = 200;

/// Represents if the TCP window scale option is enabled.
const ENABLE_WSCALE: bool = true;
/// Represents the max window scale of the receive window.
const MAX_RECV_WSCALE: u8 = 8;

/// Represents if the TCP selective acknowledgment option is enabled.
const ENABLE_SACK: bool = true;

/// Represents the max limit of UDP port for binding in local.
const PORT_COUNT: usize = 64;

/// Represents a channel redirect traffic to the proxy of SOCKS or loopback to the source in pcap.
pub struct Redirector {
    tx: Arc<Mutex<Forwarder>>,
    is_tx_src_hardware_addr_set: bool,
    src_ip_addr: Ipv4Addr,
    local_ip_addr: Option<Ipv4Addr>,
    remote: SocketAddrV4,
    streams: HashMap<(u16, SocketAddrV4), StreamWorker>,
    tcp_recv_next_map: HashMap<(u16, SocketAddrV4), u32>,
    tcp_acknowledgement_map: HashMap<(u16, SocketAddrV4), u32>,
    tcp_duplicate_map: HashMap<(u16, SocketAddrV4), usize>,
    tcp_last_retransmission_map: HashMap<(u16, SocketAddrV4), Instant>,
    tcp_wscale_map: HashMap<(u16, SocketAddrV4), u8>,
    tcp_sack_perm_map: HashMap<(u16, SocketAddrV4), bool>,
    tcp_cache_map: HashMap<(u16, SocketAddrV4), Window>,
    datagrams: HashMap<u16, DatagramWorker>,
    /// Represents the map mapping a source port to a local port.
    datagram_map: Vec<u16>,
    /// Represents the LRU mapping a local port to a source port.
    udp_lru: LruCache<u16, u16>,
    defrag: Defraggler,
}

impl Redirector {
    /// Creates a new `Redirector`.
    pub fn new(
        tx: Arc<Mutex<Forwarder>>,
        src_ip_addr: Ipv4Addr,
        local_ip_addr: Option<Ipv4Addr>,
        remote: SocketAddrV4,
    ) -> Redirector {
        let redirector = Redirector {
            tx,
            is_tx_src_hardware_addr_set: false,
            src_ip_addr,
            local_ip_addr,
            remote,
            streams: HashMap::new(),
            tcp_recv_next_map: HashMap::new(),
            tcp_acknowledgement_map: HashMap::new(),
            tcp_duplicate_map: HashMap::new(),
            tcp_last_retransmission_map: HashMap::new(),
            tcp_wscale_map: HashMap::new(),
            tcp_sack_perm_map: HashMap::new(),
            tcp_cache_map: HashMap::new(),
            datagrams: HashMap::new(),
            datagram_map: vec![0u16; u16::MAX as usize],
            udp_lru: LruCache::new(PORT_COUNT),
            defrag: Defraggler::new(),
        };
        if let Some(local_ip_addr) = local_ip_addr {
            redirector
                .tx
                .lock()
                .unwrap()
                .set_local_ip_addr(local_ip_addr);
        }

        redirector
    }

    /// Opens an `Interface` for redirect.
    pub async fn open(&mut self, rx: &mut Receiver) -> io::Result<()> {
        loop {
            match rx.next() {
                Ok(frame) => {
                    if let Some(ref indicator) = Indicator::from(frame) {
                        if let Some(t) = indicator.network_kind() {
                            match t {
                                LayerKinds::Arp => {
                                    if let Err(ref e) = self.handle_arp(indicator) {
                                        warn!("handle {}: {}", indicator.brief(), e);
                                    }
                                }
                                LayerKinds::Ipv4 => {
                                    if let Err(ref e) = self.handle_ipv4(indicator, frame).await {
                                        warn!("handle {}: {}", indicator.brief(), e);
                                    }
                                }
                                _ => unreachable!(),
                            }
                        }
                    };
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::TimedOut {
                        thread::sleep(Duration::from_millis(TIMEDOUT_WAIT));
                        continue;
                    }
                    return Err(e);
                }
            };
        }
    }

    fn handle_arp(&mut self, indicator: &Indicator) -> io::Result<()> {
        if let Some(local_ip_addr) = self.local_ip_addr {
            if let Some(arp) = indicator.arp() {
                if arp.is_request_of(self.src_ip_addr, local_ip_addr) {
                    debug!(
                        "receive from pcap: {} ({} Bytes)",
                        indicator.brief(),
                        indicator.len()
                    );

                    // Set forwarder's hardware address
                    if !self.is_tx_src_hardware_addr_set {
                        self.tx
                            .lock()
                            .unwrap()
                            .set_src_hardware_addr(arp.src_hardware_addr());
                        self.is_tx_src_hardware_addr_set = true;
                        info!("Device {} joined the network", arp.src_hardware_addr());
                    }

                    // Send
                    self.tx.lock().unwrap().send_arp_reply()?
                }
            }
        }

        Ok(())
    }

    async fn handle_ipv4(&mut self, indicator: &Indicator, buffer: &[u8]) -> io::Result<()> {
        if let Some(ref ipv4) = indicator.ipv4() {
            let buffer_without_padding =
                &buffer[..indicator.ethernet().unwrap().len() + ipv4.total_length() as usize];
            if ipv4.src() == self.src_ip_addr {
                debug!(
                    "receive from pcap: {} ({} + {} Bytes)",
                    indicator.brief(),
                    indicator.len(),
                    buffer_without_padding.len() - indicator.len()
                );
                // Set forwarder's hardware address
                if !self.is_tx_src_hardware_addr_set {
                    self.tx
                        .lock()
                        .unwrap()
                        .set_src_hardware_addr(indicator.ethernet().unwrap().src());
                    self.is_tx_src_hardware_addr_set = true;
                    info!(
                        "Device {} joined the network",
                        indicator.ethernet().unwrap().src()
                    );
                }

                if ipv4.is_fragment() {
                    // Fragmentation
                    let frag = match self.defrag.add(indicator, buffer_without_padding) {
                        Some(frag) => frag,
                        None => return Ok(()),
                    };
                    let (indicator, buffer_without_padding) = frag.concatenate();

                    if let Some(t) = indicator.transport_kind() {
                        match t {
                            LayerKinds::Tcp => {
                                self.handle_tcp(&indicator, buffer_without_padding).await?
                            }
                            LayerKinds::Udp => {
                                self.handle_udp(&indicator, buffer_without_padding).await?
                            }
                            _ => unreachable!(),
                        }
                    }
                } else {
                    if let Some(t) = indicator.transport_kind() {
                        match t {
                            LayerKinds::Tcp => {
                                self.handle_tcp(indicator, buffer_without_padding).await?
                            }
                            LayerKinds::Udp => {
                                self.handle_udp(indicator, buffer_without_padding).await?
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_tcp(&mut self, indicator: &Indicator, buffer: &[u8]) -> io::Result<()> {
        if let Some(ref tcp) = indicator.tcp() {
            if tcp.is_rst() {
                self.handle_tcp_rst(indicator);
            } else if tcp.is_ack() {
                return self.handle_tcp_ack(indicator, buffer).await;
            } else if tcp.is_syn() {
                // Pure TCP SYN
                return self.handle_tcp_syn(indicator).await;
            } else if tcp.is_fin() {
                // Pure TCP FIN
                return self.handle_tcp_fin(indicator, buffer);
            }
        }

        Ok(())
    }

    async fn handle_tcp_ack(&mut self, indicator: &Indicator, buffer: &[u8]) -> io::Result<()> {
        if let Some(tcp) = indicator.tcp() {
            let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
            let key = (tcp.src(), dst);
            let is_exist = self.streams.get(&key).is_some();
            let is_writable = match self.streams.get(&key) {
                Some(ref stream) => !stream.is_write_closed(),
                None => false,
            };

            if is_exist {
                // ACK
                if tcp.sequence() != *self.tcp_recv_next_map.get(&key).unwrap_or(&0) {
                    trace!(
                        "TCP out of order of {} -> {} at {}",
                        tcp.src(),
                        dst,
                        tcp.sequence()
                    );
                }
                self.update_tcp_acknowledgement(indicator);
                let wscale = *self.tcp_wscale_map.get(&key).unwrap_or(&0);
                {
                    let mut tx_locked = self.tx.lock().unwrap();
                    tx_locked.invalidate_cache_to(dst, tcp.src(), tcp.acknowledgement());
                    tx_locked.set_tcp_send_window(
                        dst,
                        tcp.src(),
                        (tcp.window() as usize) << wscale as usize,
                    );
                }

                let cache = self.tcp_cache_map.entry(key).or_insert_with(|| {
                    Window::with_capacity((u16::MAX as usize) << wscale as usize, tcp.sequence())
                });
                let stream = self.streams.get_mut(&key).unwrap();
                if buffer.len() > indicator.len() {
                    // ACK
                    // Append to cache
                    let prev_recv_next = cache.recv_next();
                    let payload = cache.append(tcp.sequence(), &buffer[indicator.len()..])?;
                    if cache.recv_next() != prev_recv_next {
                        if let Some(ts) = tcp.ts() {
                            // Update timestamp only when received new data
                            self.tx.lock().unwrap().set_tcp_ts(dst, tcp.src(), ts);
                        }
                    }

                    // Window scale
                    let wscale = *self.tcp_wscale_map.get(&key).unwrap_or(&0);

                    // SACK
                    if *self.tcp_sack_perm_map.get(&key).unwrap_or(&false) {
                        let sacks = cache.filled();
                        self.tx
                            .lock()
                            .unwrap()
                            .set_tcp_sacks(dst, tcp.src(), &sacks);
                    }

                    match payload {
                        Some(payload) => {
                            // Send
                            match stream.send(payload.as_slice()).await {
                                Ok(_) => {
                                    // Update window size
                                    let mut tx_locked = self.tx.lock().unwrap();
                                    tx_locked.set_tcp_window(
                                        dst,
                                        tcp.src(),
                                        (cache.remaining_size() << wscale as usize) as u16,
                                    );

                                    // Update TCP acknowledgement
                                    tx_locked.add_tcp_acknowledgement(
                                        dst,
                                        tcp.src(),
                                        payload.len() as u32,
                                    );
                                    let recv_next_entry =
                                        self.tcp_recv_next_map.entry(key).or_insert(0);
                                    *recv_next_entry = recv_next_entry
                                        .checked_add(payload.len() as u32)
                                        .unwrap_or_else(|| {
                                            payload.len() as u32 - (u32::MAX - *recv_next_entry)
                                        });

                                    // Send ACK0
                                    // If there is a heavy traffic, the ACK reported may be inaccurate, which would results in retransmission
                                    tx_locked.send_tcp_ack_0(dst, tcp.src())?;
                                }
                                Err(e) => {
                                    // Clean up
                                    self.remove(indicator);

                                    // Send ACK/RST
                                    let mut tx_locked = self.tx.lock().unwrap();
                                    tx_locked.send_tcp_ack_rst(dst, tcp.src())?;

                                    // Clean up
                                    tx_locked.remove(dst, tcp.src());

                                    return Err(e);
                                }
                            }
                        }
                        None => {
                            // Retransmission or unordered
                            // Update window size
                            let mut tx_locked = self.tx.lock().unwrap();
                            tx_locked.set_tcp_window(
                                dst,
                                tcp.src(),
                                (cache.remaining_size() << wscale as usize) as u16,
                            );

                            // Send ACK0
                            tx_locked.send_tcp_ack_0(dst, tcp.src())?;
                        }
                    }
                } else {
                    // ACK0
                    if !is_writable && self.tx.lock().unwrap().get_cache_size(dst, tcp.src()) == 0 {
                        // LAST_ACK
                        // Clean up
                        self.remove(indicator);
                        self.tx.lock().unwrap().remove(dst, tcp.src());

                        return Ok(());
                    } else if *self.tcp_duplicate_map.get(&key).unwrap_or(&0)
                        >= DUPLICATES_BEFORE_FAST_RETRANSMISSION
                    {
                        // Duplicate ACK
                        if !tcp.is_zero_window() {
                            let is_cooled_down = match self.tcp_last_retransmission_map.get(&key) {
                                Some(ref instant) => {
                                    instant.elapsed().as_millis() < RETRANSMISSION_COOL_DOWN
                                }
                                None => false,
                            };
                            if !is_cooled_down {
                                // Fast retransmit
                                let mut is_sr = false;
                                if *self.tcp_sack_perm_map.get(&key).unwrap_or(&false) {
                                    if let Some(sacks) = tcp.sack() {
                                        if sacks.len() > 0 {
                                            // Selective retransmission
                                            self.tx.lock().unwrap().retransmit_tcp_ack_without(
                                                dst,
                                                tcp.src(),
                                                sacks,
                                            )?;
                                            is_sr = true;
                                        }
                                    }
                                }

                                if !is_sr {
                                    // Back N
                                    self.tx.lock().unwrap().retransmit_tcp_ack(dst, tcp.src())?;
                                }

                                self.tcp_duplicate_map.insert(key, 0);
                                self.tcp_last_retransmission_map.insert(key, Instant::now());
                            }
                        }
                    }
                }

                // FIN
                if tcp.is_fin() && cache.is_empty() {
                    return self.handle_tcp_fin(indicator, buffer);
                }

                // Trigger sending remaining data
                self.tx.lock().unwrap().send_tcp_ack(dst, tcp.src())?;
            } else {
                // Send RST
                self.tx.lock().unwrap().send_tcp_rst(dst, tcp.src(), None)?;
            }
        }

        Ok(())
    }

    async fn handle_tcp_syn(&mut self, indicator: &Indicator) -> io::Result<()> {
        if let Some(tcp) = indicator.tcp() {
            let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
            let key = (tcp.src(), dst);
            let is_exist = self.streams.get(&key).is_some();

            // Connect if not connected, drop if established
            if !is_exist {
                // Clean up
                self.remove(indicator);

                // Latency test
                let timer = Instant::now();

                // Connect
                let stream =
                    StreamWorker::connect(self.get_tx(), tcp.src(), dst, self.remote).await;

                let stream = match stream {
                    Ok(stream) => {
                        // Latency test result (not accurate)
                        debug!(
                            "Latency to {}: {} ms (RTT)",
                            dst,
                            timer.elapsed().as_millis()
                        );

                        let mut tx_locked = self.tx.lock().unwrap();
                        // Clean up
                        tx_locked.remove(dst, tcp.src());

                        // Initialize values
                        let mut rng = rand::thread_rng();
                        let sequence: u32 = rng.gen();
                        tx_locked.set_tcp_sequence(dst, tcp.src(), sequence);
                        tx_locked.set_tcp_acknowledgement(
                            dst,
                            tcp.src(),
                            tcp.sequence().checked_add(1).unwrap_or(0),
                        );

                        self.tcp_recv_next_map
                            .insert(key, tcp.sequence().checked_add(1).unwrap_or(u32::MAX));
                        self.tcp_acknowledgement_map
                            .insert(key, sequence.checked_sub(1).unwrap_or(u32::MAX));

                        // Options
                        if PREFER_SEND_MSS {
                            if let Some(mss) = tcp.mss() {
                                tx_locked.set_tcp_send_mss(dst, tcp.src(), mss);
                            }
                        }
                        let wscale = match ENABLE_WSCALE {
                            true => tcp.wscale(),
                            false => None,
                        };
                        if let Some(wscale) = wscale {
                            tx_locked.set_tcp_wscale(dst, tcp.src(), wscale);
                        }
                        let sack_perm = ENABLE_SACK && tcp.is_sack_perm();
                        if ENABLE_TIMESTAMP {
                            if let Some(ts) = tcp.ts() {
                                tx_locked.set_tcp_ts(dst, tcp.src(), ts);
                            }
                        }

                        // Send ACK/SYN
                        let recv_wscale = match wscale {
                            Some(wscale) => Some(min(wscale, MAX_RECV_WSCALE)),
                            None => None,
                        };
                        tx_locked.send_tcp_ack_syn(dst, tcp.src(), None, recv_wscale, sack_perm)?;

                        // Options
                        drop(tx_locked);
                        if let Some(wscale) = recv_wscale {
                            self.tcp_wscale_map.insert(key, wscale);
                        }
                        if ENABLE_SACK && sack_perm {
                            self.perm_tcp_sack(indicator);
                        }

                        stream
                    }
                    Err(e) => {
                        // Clean up
                        self.remove(indicator);

                        let mut tx_locked = self.tx.lock().unwrap();
                        tx_locked.set_tcp_acknowledgement(
                            dst,
                            tcp.src(),
                            tcp.sequence().checked_add(1).unwrap_or(0),
                        );
                        if let Some(ts) = tcp.ts() {
                            tx_locked.set_tcp_ts(dst, tcp.src(), ts);
                        }
                        // Send ACK/RST
                        tx_locked.send_tcp_ack_rst(dst, tcp.src())?;

                        // Clean up
                        tx_locked.remove(dst, tcp.src());

                        return Err(e);
                    }
                };

                self.streams.insert(key, stream);
            }
        }

        Ok(())
    }

    fn handle_tcp_rst(&mut self, indicator: &Indicator) {
        if let Some(ref tcp) = indicator.tcp() {
            let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
            let key = (tcp.src(), dst);
            let is_exist = self.streams.get(&key).is_some();

            if is_exist {
                // Clean up
                self.remove(indicator);
                self.tx.lock().unwrap().remove(dst, tcp.src());
            }
        }
    }

    fn handle_tcp_fin(&mut self, indicator: &Indicator, buffer: &[u8]) -> io::Result<()> {
        if let Some(ref tcp) = indicator.tcp() {
            let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
            let key = (tcp.src(), dst);
            let is_exist = self.streams.get(&key).is_some();
            let is_readable = match self.streams.get(&key) {
                Some(ref stream) => !stream.is_read_closed(),
                None => false,
            };

            if is_exist {
                let payload_size = buffer.len() - indicator.len();
                let recv_next = tcp
                    .sequence()
                    .checked_add(payload_size as u32)
                    .unwrap_or_else(|| payload_size as u32 - (u32::MAX - tcp.sequence()));
                if recv_next == *self.tcp_recv_next_map.get(&key).unwrap_or(&0) {
                    let is_cache_empty = match self.tcp_cache_map.get(&key) {
                        Some(cache) => cache.is_empty(),
                        None => true,
                    };
                    if is_cache_empty {
                        let mut tx_locked = self.tx.lock().unwrap();
                        tx_locked.set_tcp_acknowledgement(
                            dst,
                            tcp.src(),
                            recv_next.checked_add(1).unwrap_or(0),
                        );
                        if let Some(ts) = tcp.ts() {
                            tx_locked.set_tcp_ts(dst, tcp.src(), ts);
                        }

                        let recv_next_entry = self.tcp_recv_next_map.entry(key).or_insert(0);
                        *recv_next_entry = recv_next_entry.checked_add(1).unwrap_or(u32::MAX);

                        // Send ACK0
                        tx_locked.send_tcp_ack_0(dst, tcp.src())?;
                        if is_readable {
                            // Close by local
                            let stream = self.streams.get_mut(&key).unwrap();
                            stream.close();
                        } else {
                            // Close by remote
                            // Clean up
                            tx_locked.remove(dst, tcp.src());
                            drop(tx_locked);
                            self.remove(indicator);
                        }
                    } else {
                        // Send ACK0
                        self.tx.lock().unwrap().send_tcp_ack_0(dst, tcp.src())?;
                    }
                } else {
                    trace!(
                        "TCP out of order of {} -> {} at {}",
                        tcp.src(),
                        dst,
                        tcp.sequence()
                    );
                    // Send ACK0
                    self.tx.lock().unwrap().send_tcp_ack_0(dst, tcp.src())?;
                }
            } else {
                // Send RST
                self.tx.lock().unwrap().send_tcp_rst(dst, tcp.src(), None)?;
            }
        }

        Ok(())
    }

    async fn handle_udp(&mut self, indicator: &Indicator, buffer: &[u8]) -> io::Result<()> {
        if let Some(ref udp) = indicator.udp() {
            let mut port = self.get_local_udp_port(udp.src());

            // Bind
            let is_create;
            let is_set;
            if port == 0 {
                is_create = true;
                is_set = false;
            } else {
                let worker = self.datagrams.get(&port).unwrap();
                is_create = worker.is_closed();
                is_set = worker.src_port() != udp.src();
            }
            if is_create {
                // Bind
                let (worker, bind_port) =
                    DatagramWorker::bind(self.get_tx(), udp.src(), self.remote).await?;
                self.datagrams.insert(bind_port, worker);

                // Update map and LRU
                self.datagram_map[udp.src() as usize] = bind_port;
                self.udp_lru.put(bind_port, udp.src());

                port = bind_port;

                trace!("bind UDP port {} = {}", udp.src(), port);
            } else if is_set {
                // Replace
                self.datagrams
                    .get_mut(&port)
                    .unwrap()
                    .set_src_port(udp.src());
            }

            // Send
            self.datagrams
                .get_mut(&port)
                .unwrap()
                .send_to(
                    &buffer[indicator.len()..],
                    SocketAddrV4::new(udp.dst_ip_addr(), udp.dst()),
                )
                .await?;
        }

        Ok(())
    }

    fn update_tcp_acknowledgement(&mut self, indicator: &Indicator) {
        if let Some(tcp) = indicator.tcp() {
            let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
            let key = (tcp.src(), dst);

            let record_acknowledgement = *self.tcp_acknowledgement_map.get(&key).unwrap_or(&0);
            let sub_acknowledgement = tcp
                .acknowledgement()
                .checked_sub(record_acknowledgement)
                .unwrap_or_else(|| tcp.acknowledgement() + (u32::MAX - record_acknowledgement));

            if sub_acknowledgement == 0 {
                // Duplicate
                let entry = self.tcp_duplicate_map.entry(key).or_insert(0);
                *entry = entry.checked_add(1).unwrap_or(usize::MAX);
                trace!(
                    "duplicate TCP acknowledgement of {} -> {} at {}",
                    tcp.src(),
                    dst,
                    tcp.acknowledgement()
                );
            } else if sub_acknowledgement <= MAX_U32_WINDOW_SIZE as u32 {
                self.tcp_acknowledgement_map
                    .insert(key, tcp.acknowledgement());

                self.tcp_duplicate_map.insert(key, 0);
                trace!(
                    "set TCP acknowledgement of {} -> {} to {}",
                    tcp.src(),
                    dst,
                    tcp.acknowledgement()
                );
            }
        }
    }

    fn perm_tcp_sack(&mut self, indicator: &Indicator) {
        if let Some(tcp) = indicator.tcp() {
            let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
            let key = (tcp.src(), dst);

            let sack_perm = tcp.is_sack_perm();
            if sack_perm {
                self.tcp_sack_perm_map.insert(key, true);
                trace!("permit TCP sack of {} -> {}", tcp.src(), dst);
            }
        }
    }

    fn remove(&mut self, indicator: &Indicator) {
        if let Some(tcp) = indicator.tcp() {
            let dst = SocketAddrV4::new(tcp.dst_ip_addr(), tcp.dst());
            let key = (tcp.src(), dst);

            self.streams.remove(&key);
            self.tcp_recv_next_map.remove(&key);
            self.tcp_acknowledgement_map.remove(&key);
            self.tcp_duplicate_map.remove(&key);
            self.tcp_last_retransmission_map.remove(&key);
            self.tcp_wscale_map.remove(&key);
            self.tcp_sack_perm_map.remove(&key);
            if let Some(cache) = self.tcp_cache_map.get(&key) {
                if !cache.is_empty() {
                    trace!(
                        "cache {} -> {} was removed while the cache is not empty",
                        tcp.src(),
                        dst
                    );
                }
            }
            self.tcp_cache_map.remove(&key);
            trace!("remove {} -> {}", tcp.src(), dst);
        }
    }

    fn get_tx(&self) -> Arc<Mutex<Forwarder>> {
        Arc::clone(&self.tx)
    }

    fn get_local_udp_port(&mut self, src_port: u16) -> u16 {
        let local_port = self.datagram_map[src_port as usize];
        if local_port == 0 {
            if self.udp_lru.len() < self.udp_lru.cap() {
                0
            } else {
                let pair = self.udp_lru.pop_lru().unwrap();
                let local_port = pair.0;
                let prev_src_port = pair.1;

                if prev_src_port != 0 {
                    // Reuse
                    self.datagram_map[prev_src_port as usize] = 0;
                    trace!(
                        "reuse UDP port {} = {} to {} = {}",
                        prev_src_port,
                        local_port,
                        src_port,
                        local_port
                    );
                }
                self.datagram_map[src_port as usize] = local_port;

                // Update LRU
                self.udp_lru.put(local_port, src_port);

                local_port
            }
        } else {
            // Update LRU
            self.udp_lru.get(&local_port);

            local_port
        }
    }
}
