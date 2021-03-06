use super::*;
use libc::c_void;
use std::sync::Arc;
use std::mem;

use smoltcp::wire::{EthernetAddress, EthernetProtocol, EthernetFrame};
use smoltcp::wire::{IpProtocol, IpAddress, Ipv4Repr, Ipv4Packet, Ipv4Address};
use smoltcp::{Error, Result};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{UdpRepr, UdpPacket};
use smoltcp::time::Instant;
use smoltcp::iface::{FragmentSet, FragmentedPacket};

/// Custom implementation of a mutex struct
/// Basically a wrapper around seL4/Camkes lock/unlock calls
#[derive(Debug)]
pub struct Mutex {
    inner_lock: unsafe extern "C" fn(),
    inner_unlock: unsafe extern "C" fn(),
}

impl Mutex {
    pub fn new(lock: unsafe extern "C" fn(), unlock: unsafe extern "C" fn()) -> Mutex {
        Mutex {
            inner_lock: lock,
            inner_unlock: unlock,
        }
    }

    pub fn lock(&self) {
        unsafe {
            (self.inner_lock)();
        }
    }

    pub fn unlock(&self) {
        unsafe {
            (self.inner_unlock)();
        }
    }
}

pub struct ExternalFirewallWrapper {
    f: unsafe extern "C" fn(u32, u16, u32, u16, u16, *const u8, u16) -> i32,
}

impl ExternalFirewallWrapper {
    pub fn new(
        f: unsafe extern "C" fn(u32, u16, u32, u16, u16, *const u8, u16) -> i32,
    ) -> ExternalFirewallWrapper {
        ExternalFirewallWrapper { f: f }
    }

    pub fn call(
        &self,
        src_addr: u32,
        src_port: u16,
        dst_addr: u32,
        dst_port: u16,
        payload_len: u16,
        payload: *const u8,
        max_payload_len: u16,
    ) -> i32 {
        unsafe {
            (self.f)(
                src_addr,
                src_port,
                dst_addr,
                dst_port,
                payload_len,
                payload,
                max_payload_len,
            )
        }
    }
}

/// Declare static mutexes we wish to use
/// This will get initialised the first time a thread tries to access the internal data.
/// lazy_statics use atomic spinlocks to ensure that the structures are only initialised once.
lazy_static! {
    /// client/ethdriver protection
    static ref MTX_ETHDRIVER_BUF: Arc<Mutex> = Arc::new(Mutex::new(externs::ethdriver_buf_lock, externs::ethdriver_buf_unlock));
    static ref MTX_CLIENT_BUF: Arc<Mutex> = Arc::new(Mutex::new(externs::client_buf_lock, externs::client_buf_unlock));

    /// a wrapper for `packet_in`
    pub static ref FN_PACKET_IN: Arc<camkesrust::Mutex<ExternalFirewallWrapper>> = {
        let inner = ExternalFirewallWrapper::new(externs::packet_in);
        Arc::new(camkesrust::Mutex::new(inner).unwrap())
    };

    /// a wrapper for `packet_out`
    pub static ref FN_PACKET_OUT: Arc<camkesrust::Mutex<ExternalFirewallWrapper>> = {
        let inner = ExternalFirewallWrapper::new(externs::packet_out);
        Arc::new(camkesrust::Mutex::new(inner).unwrap())
    };

    /// fragments on rx side
    pub static ref FRAGMENTS_RX: Arc<camkesrust::Mutex<FragmentSet<'static>>> = {
        let mut fragments = FragmentSet::new(vec![]);
        for _idx in 0..constants::SUPPORTED_FRAGMENTS {
            let fragment = FragmentedPacket::new(vec![0; constants::MAX_REASSEMBLED_FRAGMENT_SIZE]);
            fragments.add(fragment);
        }
        Arc::new(camkesrust::Mutex::new(fragments).unwrap())
    };

    /// fragments on tx side
    pub static ref FRAGMENTS_TX: Arc<camkesrust::Mutex<FragmentSet<'static>>> = {
        let mut fragments = FragmentSet::new(vec![]);
        for _idx in 0..constants::SUPPORTED_FRAGMENTS {
            let fragment = FragmentedPacket::new(vec![0; constants::MAX_REASSEMBLED_FRAGMENT_SIZE]);
            fragments.add(fragment);
        }
        Arc::new(camkesrust::Mutex::new(fragments).unwrap())
    };

    /// enqued eth_frames to be send
    pub static ref PACKETS_TX: Arc<camkesrust::Mutex<Vec<Vec<u8>>>> = Arc::new(camkesrust::Mutex::new(vec![]).unwrap());

    /// enqued eth_frames to be passed to the client
    pub static ref PACKETS_RX: Arc<camkesrust::Mutex<Vec<Vec<u8>>>> = Arc::new(camkesrust::Mutex::new(vec![]).unwrap());

    /// kludge to prevent reentrancy around client_rx/tx calls
    pub static ref RET_CLIENT_TX: Arc<camkesrust::Mutex<i32>> = Arc::new(camkesrust::Mutex::new(-1).unwrap());
    pub static ref RET_CLIENT_RX: Arc<camkesrust::Mutex<i32>> = Arc::new(camkesrust::Mutex::new(-1).unwrap());

    /// Our mac address won't change at runtime, so we will save the value once we know it.
    pub static ref CLIENT_MAC_ADDRESS:EthernetAddress = get_device_mac();

}

/// A safe wrapper around `client_buf` ptr
pub fn client_buf_value() -> *mut c_void {
    unsafe {
        let val = externs::client_buf(1);
        assert!(!val.is_null());
        val
    }
}

/// A safe wrapper around `ethdriver_buf` ptr
pub fn ethdriver_buf_value() -> *mut c_void {
    unsafe {
        let val = externs::ethdriver_buf;
        assert!(!val.is_null());
        val
    }
}

/// Generic insertion of `data` into a `buffer`, returns number of inserted bytes
fn sel4_buffer_insert(data: Vec<u8>, buffer: *mut c_void) -> usize {
    unsafe {
        let len = data.len();
        assert!(!buffer.is_null());
        assert!(len < constants::BUFFER_SIZE);
        let buf_ptr = std::mem::transmute::<*mut c_void, *mut u8>(buffer);
        let slice = std::slice::from_raw_parts_mut(buf_ptr, len);
        slice[..].clone_from_slice(data.as_slice());
        slice.len()
    }
}

/// Generic fetch of `len` bytes from `buffer`
fn sel4_buffer_fetch(len: usize, buffer: *mut c_void) -> Vec<u8> {
    unsafe {
        assert!(!buffer.is_null());
        assert!(len < constants::BUFFER_SIZE);
        // create a slice of length `len` from the buffer
        let local_buf_ptr = std::mem::transmute::<*mut c_void, *mut u8>(buffer);
        let slice = std::slice::from_raw_parts(local_buf_ptr, len);
        let mut v = Vec::with_capacity(slice.len());
        v.extend_from_slice(slice);
        v
    }
}

/// attempt to send `data` to the outside world
/// return 0 if data were successfully queued to the ethdriver
/// return -1 otherwise
/// Note that we don't know if the data were transmitted, as the ethdriver
/// doesn't provide a notification for that
pub fn dispatch_data_to_ethdriver(data: Vec<u8>) -> i32 {
    MTX_ETHDRIVER_BUF.lock();
    let len = sel4_buffer_insert(data, ethdriver_buf_value());
    let ret = unsafe { externs::ethdriver_tx(len as i32) };
    MTX_ETHDRIVER_BUF.unlock();
    ret
}

/// Possible return values from calling `ethdriver_rx` and subsequent
/// `sel4_buffer_fetch()`
#[derive(Debug)]
pub struct EthdriverRxStatus {
    finished: bool,
}

impl EthdriverRxStatus {
    pub fn new() -> EthdriverRxStatus {
        EthdriverRxStatus {finished: false}
    }
}
impl Iterator for EthdriverRxStatus {

    type Item = Vec<u8>;
    /// Attempt to recieve data from the ethdriver
    fn next(&mut self) -> Option<Vec<u8>> {
        if self.finished {
            return None;
        }
        MTX_ETHDRIVER_BUF.lock();
        let mut len: i32 = 0;
        let ret = unsafe { externs::ethdriver_rx(&mut len) };

        let status = match ret {
            -1 => None, // no data available
            e @ 0 ... 1 => { // Data available
                if let 0 = e {
                    self.finished = true;  // This is the last packet available
                }
                let data = sel4_buffer_fetch(len as usize, ethdriver_buf_value());
                Some(data)

            }
            _ => panic!("Unexpected return value from ethdriver_rx"),
        };
        MTX_ETHDRIVER_BUF.unlock();
        status
    }
}

/// copy `data` to client_buffer, return the length of the enqueued data
pub fn copy_data_to_client_buf(data: Vec<u8>) -> i32 {
    MTX_CLIENT_BUF.lock();
    let val = sel4_buffer_insert(data, client_buf_value());
    MTX_CLIENT_BUF.unlock();
    val as i32
}

/// copy `len` bytes from client buffer and return as `Vec<u8>`
pub fn fetch_client_data(len: usize) -> Vec<u8> {
    MTX_CLIENT_BUF.lock();
    let data = sel4_buffer_fetch(len, client_buf_value());
    MTX_CLIENT_BUF.unlock();
    data
}

/// Pass the device MAC address to the callee
pub fn get_device_mac() -> EthernetAddress {
    let mut b1: u8 = 0;
    let mut b2: u8 = 0;
    let mut b3: u8 = 0;
    let mut b4: u8 = 0;
    let mut b5: u8 = 0;
    let mut b6: u8 = 0;

    unsafe {
        externs::ethdriver_mac(&mut b1, &mut b2, &mut b3, &mut b4, &mut b5, &mut b6);
    }

    EthernetAddress([b1, b2, b3, b4, b5, b6])
}

/// Returns a new ID for a set of fragmented packets
/// Note that this would normally be a regular random
/// number generator, but sadly, seL4 + Rust doesn't have
/// the right bindings for it (yet)
pub fn get_pseudorandom_packet_id() -> u16 {
    static mut SEED: u16 = 42;
    unsafe {
        SEED += 1;
        SEED
    }
}

/// Get a "fake" timestamp to help purge fragmnets set
fn timestamp() -> Instant {
    static mut MS: i64 = 0;
    unsafe {
        MS += 1; // helper for managing too-old fragments
    }
    let timestamp = unsafe { Instant::from_millis(MS) };
    timestamp
}

/// Return OK if an eth_packet was enqued to the packet buffer,
/// otherwise return an error message
/// The program flow is as follows:
/// Check EtherType:
///		- Arp: pass through (enqueue directly)
///     - Ipv4: check further:
///				- 0 to N packedts returned: enqueue to `packet_buffer`
///				- error returned: propagate error
///     - other: drop
pub fn process_ethernet(
    frame: Vec<u8>,
    packet_buffer: Arc<camkesrust::Mutex<Vec<Vec<u8>>>>,
    fragment_buffer: Arc<camkesrust::Mutex<FragmentSet<'static>>>,
    external_firewall_fn: Arc<camkesrust::Mutex<ExternalFirewallWrapper>>,
    check_mac: bool,
) -> Result<()> {
    let eth_frame = EthernetFrame::new_checked(frame)?;

    if check_mac {
        // Ignore any packets not directed at our hardware address.
        let local_ethernet_addr = *CLIENT_MAC_ADDRESS;
        debug_print!(
            "Firewall process_ethernet: local eth addr: {}, destinatione th address: {}",
            local_ethernet_addr,
            eth_frame.dst_addr()
        );
        //#[cfg(feature = "mac-check")]
        {
            // check the MAC address of the incoming frame
            if !eth_frame.dst_addr().is_broadcast() && !eth_frame.dst_addr().is_multicast()
                && eth_frame.dst_addr() != local_ethernet_addr
            {
                debug_print!("Firewall process_ethernet: The packet wasn't for us, quitely drop it");
                return Ok(());
            }
        }
    }

    debug_print!("Firewall process_ethernet: EthernetProtocol = {}",
        eth_frame.ethertype()
    );

    match eth_frame.ethertype() {
        EthernetProtocol::Ipv4 => {
            debug_print!("Firewall process_ethernet: processing IPv4");
            match process_ipv4(eth_frame, fragment_buffer, external_firewall_fn) {
                Ok(mut packets) => {
                    // enqueue frames
                    let mut buffer = packet_buffer.lock();
                    while !packets.is_empty() && buffer.len() < constants::MAX_ENQUEUED_PACKETS {
                        let eth_frame = packets.remove(0);
                        buffer.push(eth_frame.into_inner());
                    }
                }
                Err(e) => return Err(e),
            }
        }
        EthernetProtocol::Ipv6 => {
            // Ipv6 traffic is not allowed
            debug_print!("Firewall process_ethernet: dropping IPV6 traffic");
        }
        EthernetProtocol::Arp => {
            // Arp traffic is allowed, pass-through
            debug_print!("process_ethernet client_tx: passing through ARP traffic");
            // enqueue unchanged frame
            let mut buffer = packet_buffer.lock();
            if buffer.len() < constants::MAX_ENQUEUED_PACKETS {
                buffer.push(eth_frame.into_inner());    
            }
        }
        _ => {
            // drop unrecognized protocol
            debug_print!("Firewall process_ethernet: drop unrecognized eth protocol");
        }
    }

    Ok(())
}

/// A helper function that splits a large IPv4 packet into multiple fragmented
/// packets that fit MTU

fn fragment_large_udp_packet(
    udp_packet: UdpPacket<Vec<u8>>,
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    packet_id: u16,
) -> Result<Vec<Ipv4Packet<Vec<u8>>>> {
    // initialize variables
    let udp_packet = udp_packet.into_inner();
    let mut start_len = 0;
    let mut end_len = constants::MTU_UDP;
    let mut fragment_offset = 0;
    let mut remaining_len = udp_packet.len();
    let mut packet_id = packet_id;

    let mut ipv4_packet_buffer = vec![];

    if remaining_len < end_len {
        let ip_repr = Ipv4Repr {
            src_addr: src_addr,
            dst_addr: dst_addr,
            protocol: IpProtocol::Udp,
            payload_len: udp_packet.len(),
            hop_limit: 64,
        };
        let ip_packet = {
            let mut ip_packet = Ipv4Packet::new(vec![0; ip_repr.buffer_len() + udp_packet.len()]);
            ip_repr.emit(&mut ip_packet, &ChecksumCapabilities::default());
            ip_packet.set_ident(packet_id);
            ip_packet
                .payload_mut()
                .copy_from_slice(udp_packet.as_slice());
            ip_packet.fill_checksum();
            ip_packet
        };
        ipv4_packet_buffer.push(ip_packet);
        return Ok(ipv4_packet_buffer);
    } else {
        if packet_id == 0 {
            packet_id = get_pseudorandom_packet_id();
        }
        {
            // create the first packet
            let ip_repr = Ipv4Repr {
                src_addr: src_addr,
                dst_addr: dst_addr,
                protocol: IpProtocol::Udp,
                payload_len: constants::MTU_UDP,
                hop_limit: 64,
            };
            let ip_packet = {
                let mut ip_packet =
                    Ipv4Packet::new(vec![0; ip_repr.buffer_len() + constants::MTU_UDP]);
                ip_repr.emit(&mut ip_packet, &ChecksumCapabilities::default());
                ip_packet
                    .payload_mut()
                    .copy_from_slice(&udp_packet[start_len..end_len]);
                ip_packet.set_ident(packet_id);
                ip_packet.set_frag_offset(fragment_offset); // first packet
                ip_packet.set_more_frags(true); // more fragments
                ip_packet.set_dont_frag(false);
                ip_packet.fill_checksum();
                ip_packet
            };
            ipv4_packet_buffer.push(ip_packet);
        }

        // update remaining len
        remaining_len -= constants::MTU_UDP;

        while remaining_len > constants::MTU_UDP {
            // create middle packets

            // update indices
            start_len += constants::MTU_UDP;
            end_len += constants::MTU_UDP;
            fragment_offset += constants::MTU_UDP as u16;

            let ip_repr = Ipv4Repr {
                src_addr: src_addr,
                dst_addr: dst_addr,
                protocol: IpProtocol::Udp,
                payload_len: constants::MTU_UDP,
                hop_limit: 64,
            };
            let ip_packet = {
                let mut ip_packet =
                    Ipv4Packet::new(vec![0; ip_repr.buffer_len() + constants::MTU_UDP]);
                ip_repr.emit(&mut ip_packet, &ChecksumCapabilities::default());
                ip_packet
                    .payload_mut()
                    .copy_from_slice(&udp_packet[start_len..end_len]);
                ip_packet.set_ident(packet_id);
                ip_packet.set_frag_offset(fragment_offset); // last packet
                ip_packet.set_more_frags(true); // more fragmentrs
                ip_packet.set_dont_frag(false);
                ip_packet.fill_checksum();
                ip_packet
            };
            ipv4_packet_buffer.push(ip_packet);
            // update remaining len
            remaining_len -= constants::MTU_UDP;
        }

        {
            // create the last packet
            // update indices
            start_len += constants::MTU_UDP;
            fragment_offset += constants::MTU_UDP as u16;

            let ip_repr = Ipv4Repr {
                src_addr: src_addr,
                dst_addr: dst_addr,
                protocol: IpProtocol::Udp,
                payload_len: remaining_len,
                hop_limit: 64,
            };
            let ip_packet = {
                let mut ip_packet = Ipv4Packet::new(vec![0; ip_repr.buffer_len() + remaining_len]);
                ip_repr.emit(&mut ip_packet, &ChecksumCapabilities::default());
                ip_packet
                    .payload_mut()
                    .copy_from_slice(&udp_packet[start_len..]);
                ip_packet.set_ident(packet_id);
                ip_packet.set_frag_offset(fragment_offset); // last packet
                ip_packet.set_more_frags(false); // no more fragmentrs
                ip_packet.set_dont_frag(false);
                ip_packet.fill_checksum();
                ip_packet
            };
            ipv4_packet_buffer.push(ip_packet);
        }
    }
    Ok(ipv4_packet_buffer)
}

/// If there are ETH_CRC_LEN extra bytes on the end of our ipv4 packet, this is likely the CRC from the
/// ethernet frame and need to be removed.
fn shave_crc_from_ipv4<'frame>(
    packet: Ipv4Packet<&'frame [u8]>,
) -> Result<Ipv4Packet<&'frame [u8]>> {
    let (payload_len, header_len): (usize, usize) =
        (packet.payload().len(), packet.header_len() as usize);
    let payload = packet.into_inner();

    if payload_len + header_len + constants::ETH_CRC_LEN == payload.len() {
        return Ipv4Packet::new_checked(&payload[..payload.len() - constants::ETH_CRC_LEN]);
    } else {
        // In all other cases we return the packet
        // This includes cases where the payload.len() will be rounded up to 50 if the payload
        // was below the minimum payload size supported by ethernet
        return Ipv4Packet::new_checked(payload);
    }
}

/// Return a vector of ethernet frames resulting from processing the `eth_frame`
/// Input is a single Ipv4 ethernet frame, output can be zero or more frames
/// Process frame:
///	 - check if the packet is fragmented
///		- yes: process fragment
///				Ok()   - new reassmbled packet is returned
///				None   - no new packet, return Error:Dropped
///			    Err(e) - error processing the packet
///		- no: continue
///
///  - check ipv4 protocol:
///		- ICMP/IGMP: pass through
///	    - UDP: check payload further
///				- a single UDP packet returned
///             - no packet returned, return Error:Dropped
///				- error returned: propagate error
///     - other: drop
///
///  - if Ipv4 packet > MTU, fragment the packet and enqueue the fragments
///
fn process_ipv4(
    eth_frame: EthernetFrame<Vec<u8>>,
    fragment_buffer: Arc<camkesrust::Mutex<FragmentSet<'static>>>,
    external_firewall_fn: Arc<camkesrust::Mutex<ExternalFirewallWrapper>>,
) -> Result<Vec<EthernetFrame<Vec<u8>>>> {
    // eth packet contains the original eth data
    let mut eth_packet = eth_frame.into_inner();
    // eth payload contains the payload only, and is to be modified
    let mut eth_payload = {
        let mut payload = vec![];
        payload.extend_from_slice(&eth_packet[constants::ETHERNET_FRAME_PAYLOAD..]);
        payload
    };

    // return structures
    let mut ipv4_packet_buffer: Vec<Ipv4Packet<Vec<u8>>> = vec![];
    let mut eth_packet_buffer: Vec<EthernetFrame<Vec<u8>>> = vec![];
    {
        let eth_frame = EthernetFrame::new_checked(&eth_packet)?;
        debug_print!("Firewall process_ipv4: eth_fram payload len = {}", eth_frame.payload().len());

        {
            // process only UDP fragments
            let ipv4_packet = shave_crc_from_ipv4(Ipv4Packet::new_checked(eth_frame.payload())?)?;
            //let ipv4_packet = Ipv4Packet::new_checked(eth_frame.payload())?;
            if (ipv4_packet.more_frags() || ipv4_packet.frag_offset() > 0)
                && ipv4_packet.protocol() == IpProtocol::Udp
            {
                debug_print!("Firewall process_ipv4: fragmented packet detected");
                let mut fragments = fragment_buffer.lock();
                match process_ipv4_fragment(ipv4_packet, timestamp(), &mut fragments)? {
                    Some(assembled_ipv4_payload) => {
                        eth_payload = assembled_ipv4_payload;
                    }
                    None => return Err(Error::Fragmented),
                }
            }
        }

        let ipv4_packet = shave_crc_from_ipv4(Ipv4Packet::new_checked(&eth_payload[..])?)?;
        //let ipv4_packet = shave_crc_from_ipv4(Ipv4Packet::new_checked(eth_frame.payload())?)?;
        //let ipv4_packet = Ipv4Packet::new_checked(eth_frame.payload())?;
        let checksum_caps = ChecksumCapabilities::default();
        let ipv4_repr = Ipv4Repr::parse(&ipv4_packet, &checksum_caps)?;

        debug_print!("Firewall process_ipv4: ipv4 protocol = {}", ipv4_repr.protocol);

        match ipv4_repr.protocol {
            IpProtocol::Icmp => {
                // passthrough
                debug_print!("Firewall process_ipv4: ICMP protocol, returning unchanged");
            }
            IpProtocol::Igmp => {
                //* passthrough
                debug_print!("Firewall process_ipv4: I protocol, returning unchanged");
            }
            IpProtocol::Udp => {
                // check with external firewall
                debug_print!("Firewall process_ipv4: UDP protocol, parsing further");
                let ident = ipv4_packet.ident();
                match process_udp(ipv4_repr, ipv4_packet.payload(), external_firewall_fn) {
                    Ok(udp_packet) => {
                        debug_print!("Firewall process_ipv4: UDP packet returned, parsing/fragmenting");
                        match fragment_large_udp_packet(
                            udp_packet,
                            ipv4_packet.src_addr(),
                            ipv4_packet.dst_addr(),
                            ident,
                        ) {
                            Ok(mut ipv4_packets) => {
                                debug_print!(
                                    "Firewall process_ipv4: have {} UDP packets, appending buffer",
                                    ipv4_packets.len()
                                );
                                ipv4_packet_buffer.append(&mut ipv4_packets);
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Err(e) => {
                        // drop packet
                        let e = Err(e);
                        debug_print!(
                            "Firewall process_ipv4: drop UDP packet, return {:?}",
                            e
                        );
                        return e;
                    }
                }
            }
            _ => {
                // unknown protocol, drop packet
                let e = Err(Error::Unrecognized);
                debug_print!(
                    "Firewall process_ipv4: Unknown protocol, returning error = {:?}",
                    e
                );
                return e;
            }
        }
    }

    if ipv4_packet_buffer.is_empty() {
        debug_print!("Firewall process_ipv4: no data were changed, simply copy over the original data");
        eth_packet_buffer.push(EthernetFrame::new_checked(eth_packet)?);
    } else {
        debug_print!("Firewall process_ipv4: we have 1 to N Ipv4 packets we need to enqueue");
        let mut _cnt = 0;
        while !ipv4_packet_buffer.is_empty() {
            _cnt += 1;
            debug_print!("Firewall process_ipv4: enqued {} packets", _cnt);
            let ipv4_packet = ipv4_packet_buffer.remove(0);
            eth_packet.truncate(constants::ETHERNET_FRAME_PAYLOAD);
            eth_packet.append(&mut ipv4_packet.into_inner());
            eth_packet_buffer.push(EthernetFrame::new_checked(eth_packet.clone())?);
        }
    }

    Ok(eth_packet_buffer)
}

/// Process an IPv4 fragment
/// Returns etiher a vector representing an assembled packet,
/// nothing (in case no packets are available),
/// or and error caused by fragment processing
fn process_ipv4_fragment<'frame, 'r>(
    ipv4_packet: Ipv4Packet<&'frame [u8]>,
    timestamp: Instant,
    fragments: &'r mut FragmentSet<'static>,
) -> Result<Option<Vec<u8>>> {
    debug_print!("Firewall process_ipv4_fragment: got a fragment with id = {}", ipv4_packet.ident());
    // get an existing fragment or attempt to get a new one
    let fragment = match fragments.get_packet(
        ipv4_packet.ident(),
        ipv4_packet.src_addr(),
        ipv4_packet.dst_addr(),
        timestamp,
    ) {
        Some(frag) => frag,
        None => return Err(Error::FragmentSetFull),
    };

    if fragment.is_empty() {
        // this is a new packet
        debug_print!("Firewall process_ipv4_fragment: fragment is empty");
        fragment.start(
            ipv4_packet.ident(),
            ipv4_packet.src_addr(),
            ipv4_packet.dst_addr(),
        );
    }

    if !ipv4_packet.more_frags() {
        // last fragment, remember data length
        debug_print!("Firewall process_ipv4_fragment: this is the last fragment");
        fragment
            .set_total_len(ipv4_packet.frag_offset() as usize + ipv4_packet.total_len() as usize);
    }

    match fragment.add(
        ipv4_packet.header_len() as usize,
        ipv4_packet.frag_offset() as usize,
        ipv4_packet.payload().len(),
        ipv4_packet.into_inner(),
        timestamp,
    ) {
        Ok(_) => {
            debug_print!("Firewall process_ipv4_fragment: adding fragment OK");
        }
        Err(_e) => {
            debug_print!("Firewall process_ipv4_fragment: adding fragment error {:?}", _e);
            fragment.reset();
            return Err(Error::TooManyFragments);
        }
    }

    if fragment.check_contig_range() {
        // this is the last packet, attempt reassembly
        let front = match fragment.front() {
            Some(f) => {
                debug_print!("Firewall process_ipv4_fragment: fragment reassembly Some");
                f
            }
            None => {
                debug_print!("Firewall process_ipv4_fragment: fragment reassebly None, return Ok(None)");
                return Ok(None);
            }
        };
        {
            // because the different mutability of the underlying buffers, we have to do this exercise
            let mut ipv4_packet = Ipv4Packet::new_checked(fragment.get_buffer_mut(0, front))?;
            ipv4_packet.set_total_len(front as u16);
            ipv4_packet.fill_checksum();
        }
        let ret = {
            let mut ret = vec![0; front];
            ret.clone_from_slice(fragment.get_buffer(0, front));
            ret
        };
        fragment.reset();
        return Ok(Some(ret));
    }

    // not the last fragment
    let r = Ok(None);
    debug_print!("Firewall process_ipv4_fragment: this wasn't the last fragment, returning {:?}", r);
    return r;
}

/// Process UDP data and eithe return an DP packet approved by the external firewall,
/// or an error (including Error:Dropped)
/// The processing is following:
/// - parse UDP packet
/// - create a new vector with the payload
/// - call external firewall (if not NULL)
/// - if approved, assembled a new UDP packet
/// - otherwise return Error
fn process_udp<'frame>(
    ip_repr: Ipv4Repr,
    ip_payload: &'frame [u8],
    external_firewall_fn: Arc<camkesrust::Mutex<ExternalFirewallWrapper>>,
) -> Result<UdpPacket<Vec<u8>>> {
    let udp_packet = UdpPacket::new_checked(ip_payload)?;
    let checksum_caps = ChecksumCapabilities::default();
    let _udp_repr = UdpRepr::parse(
        &udp_packet,
        &IpAddress::from(ip_repr.src_addr),
        &IpAddress::from(ip_repr.dst_addr),
        &checksum_caps,
    )?; // to force checksum

    // get proper addresses
    let src_addr_bytes = {
        let mut bytes = [0, 0, 0, 0];
        bytes[..].clone_from_slice(ip_repr.src_addr.as_bytes());
        let bytes = unsafe { std::mem::transmute::<[u8; 4], u32>(bytes) };
        bytes
    };

    let dst_addr_bytes = {
        let mut bytes = [0, 0, 0, 0];
        bytes[..].clone_from_slice(ip_repr.dst_addr.as_bytes());
        let bytes = unsafe { std::mem::transmute::<[u8; 4], u32>(bytes) };
        bytes
    };

    // prepare data
    let mut udp_data = Vec::with_capacity(constants::MAX_UDP_PAYLOAD_SIZE);
    udp_data.extend_from_slice(udp_packet.payload());
    let data_len = udp_data.len();
    let max_data_len = udp_data.capacity();
    let data_ptr = udp_data.as_mut_ptr();

    // call external firewall
    debug_print!(
        "Firewall process_udp: calling external firewall.
        src_addr = {},
        udp_packet.src_port = {},
        dst_addr = {},
        udp_packet.dst_port = {},
        udp payload len = {}
        buffer size = {}",
        ip_repr.src_addr,
        udp_packet.src_port(),
        ip_repr.dst_addr,
        udp_packet.dst_port(),
        data_len as u16,
        max_data_len as u16,
    );

    let payload_len = external_firewall_fn.lock().call(
        src_addr_bytes,
        udp_packet.src_port(),
        dst_addr_bytes,
        udp_packet.dst_port(),
        data_len as u16,
        data_ptr,
        max_data_len as u16,
    );

    // update vector
    unsafe {
        mem::forget(udp_data);
        udp_data = Vec::from_raw_parts(data_ptr, payload_len as usize, max_data_len);
    }

    if payload_len > 0 && payload_len as usize <= constants::MAX_UDP_PACKET_SIZE {
        debug_print!("Firewall process_udp: packet approved, reassembling with payload len = {}",
            payload_len
        );
        let udp_repr = UdpRepr {
            src_port: udp_packet.src_port(),
            dst_port: udp_packet.dst_port(),
            payload: &udp_data,
        };
        let mut udp_packet_data = vec![0; udp_repr.buffer_len()];
        {
            let mut udp_packet = UdpPacket::new(udp_packet_data.as_mut_slice());
            udp_repr.emit(
                &mut udp_packet,
                &IpAddress::from(ip_repr.src_addr),
                &IpAddress::from(ip_repr.dst_addr),
                &ChecksumCapabilities::default(),
            );
            udp_packet.fill_checksum(
                &IpAddress::from(ip_repr.src_addr),
                &IpAddress::from(ip_repr.dst_addr),
            );
        }

        let r = Ok(UdpPacket::new_checked(udp_packet_data)?);
        debug_print!("Firewall process_udp: udp packet created, returning OK");
        return r;
    } else {
        let e = Err(Error::Dropped);
        debug_print!("Firewall process_udp: packet dropped, returning {:?}", e);
        return e;
    }
}
