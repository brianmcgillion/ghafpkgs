/*
    Copyright 2025 TII (SSRC) and the contributors
    SPDX-License-Identifier: Apache-2.0
*/

// forward.rs

use lazy_static::lazy_static;
use log::{debug, error, info, trace};
use pnet::datalink::NetworkInterface;
use pnet::ipnetwork::IpNetwork;
use pnet::packet::arp::ArpPacket;
use pnet::packet::ethernet::EtherTypes;
use pnet::packet::ethernet::MutableEthernetPacket;
use pnet::packet::icmp::IcmpPacket;
use pnet::packet::ip::IpNextHeaderProtocols;
use pnet::packet::ipv4::Ipv4Packet;
use pnet::packet::ipv4::MutableIpv4Packet;
use pnet::packet::ipv6::Ipv6Packet;
use pnet::packet::tcp;
use pnet::packet::tcp::{MutableTcpPacket, TcpPacket};
use pnet::packet::udp;
use pnet::packet::udp::{MutableUdpPacket, UdpPacket};
use pnet::packet::MutablePacket;
use pnet::packet::Packet;
use pnet::util::MacAddr;
use std::error::Error;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::Mutex;
pub mod forward {
    use super::*;
    /// Holds the network interface details, including external and internal IPs and MAC addresses.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Ifaces {
        pub ext_ip: IpNetwork,
        pub ext_mac: MacAddr,
        pub int_ip: IpNetwork,
        pub int_mac: MacAddr,
    }
    lazy_static! {
        static ref IFACES: RwLock<Ifaces> = RwLock::new(Ifaces {
            ext_ip: IpNetwork::V4("0.0.0.0/0".parse().unwrap()),
            ext_mac: MacAddr::zero(),
            int_ip: IpNetwork::V4("0.0.0.0/0".parse().unwrap()),
            int_mac: MacAddr::zero(),
        });
    }
    /// Assigns the external and internal network interfaces and their respective IPs and MAC addresses.
    ///
    /// # Arguments
    /// * `ext_iface` - The external network interface.
    /// * `int_iface` - The internal network interface.
    /// * `ext_iface_ip` - The external IP address to assign (optional).
    /// * `int_iface_ip` - The internal IP address to assign (optional).
    ///
    /// # Returns
    /// A `Result` indicating success or failure of the assignment.
    pub fn assign_ifaces(
        ext_iface: &NetworkInterface,
        int_iface: &NetworkInterface,
        ext_iface_ip: Option<IpNetwork>,
        int_iface_ip: Option<IpNetwork>,
    ) -> Result<(), String> {
        let ext_ip = select_ip(ext_iface, ext_iface_ip)?;
        let int_ip = select_ip(int_iface, int_iface_ip)?;

        let mut ifaces = IFACES.write().unwrap();
        ifaces.ext_ip = ext_ip;
        ifaces.ext_mac = ext_iface.mac.unwrap_or_default();
        ifaces.int_ip = int_ip;
        ifaces.int_mac = int_iface.mac.unwrap_or_default();
        Ok(())
    }

    fn select_ip(
        iface: &NetworkInterface,
        iface_ip: Option<IpNetwork>,
    ) -> Result<IpNetwork, String> {
        if iface.ips.iter().filter(|ip| ip.is_ipv4()).count() > 1 {
            if let Some(ip) = iface_ip {
                if !iface.ips.iter().any(|iface_ip| iface_ip.ip() == ip.ip()) {
                    return Err(format!(
                        "Provided IP {} does not match any IPs in interface {}",
                        ip, iface.name
                    ));
                }
                return Ok(ip);
            }
        }

        iface
            .ips
            .iter()
            .find_map(|ip| {
                if let IpNetwork::V4(ipv4) = ip {
                    Some(IpNetwork::V4(*ipv4))
                } else {
                    None
                }
            })
            .ok_or_else(|| format!("No IPv4 address found for interface {}", iface.name))
    }
    /// Retrieves the current network interface details (external and internal IP and MAC).
    ///
    /// # Returns
    /// An `Ifaces` structure containing the external and internal IPs and MACs.
    pub fn get_ifaces() -> Ifaces {
        // Acquire a read lock to access IFACES
        let ifaces = IFACES
            .read()
            .expect("Failed to acquire read lock on IFACES");
        ifaces.clone()
    }
    /// Processes a packet coming from the external interface and forwards it to the internal network.
    ///
    /// # Arguments
    /// * `tx` - The data link sender used to transmit the packet.
    /// * `eth_packet` - The Ethernet packet to forward.
    /// * `src_ips` - A vector of source IP addresses to check.
    /// * `src_mac` - The source MAC address.
    /// * `dest_mac` - The destination MAC address.
    /// * `dest_ip` - The destination IP address.
    pub async fn external_to_internal_process_packet(
        tx: Arc<Mutex<Box<dyn pnet::datalink::DataLinkSender>>>,
        eth_packet: &mut MutableEthernetPacket<'_>,
        src_ips: &Vec<pnet::ipnetwork::IpNetwork>,
        src_mac: MacAddr,
        dest_mac: MacAddr,
        dest_ip: IpNetwork,
    ) {
        let mut tx = tx.lock().await; // Acquire lock asynchronously

        /*
        1) src_ip -> should remain as it is
        2) dest_ip,dest mac -> modified with chrome-vm ip
        3) calculate crc and checksums again
        */
        let is_ipv6: bool = eth_packet.get_ethertype() == EtherTypes::Ipv6;
        if is_ipv6 || is_it_own_packet(eth_packet, src_ips) || !is_packet_safe(eth_packet) {
            debug!("Ext to Int - packet dropped {}", parse_packet(eth_packet));
        } else if modify_ext_to_int_packet(eth_packet, src_mac, dest_mac, dest_ip) {
            // println!(
            //     "forwarded_packet:{:?}, len:{}",
            //     forwarded_packet,
            //     forwarded_packet.len()
            // );

            // println!(
            //     "forwarded_packet_as_slice:{:?}",
            //     forwarded_packet.as_slice()
            // );
            match tx.send_to(eth_packet.packet(), None) {
                Some(Ok(_)) => {
                    info!(
                        "Ext to Int - Forwarded packet: {}",
                        parse_packet(eth_packet)
                    );
                    trace!("Ext to Int - Forwarded packet: {:?}", eth_packet);
                }
                Some(Err(e)) => {
                    error!("Error sending packet: {}", e);
                }
                None => error!("Error: Send failed, no destination address."),
            }
        }
    }
    /// Determines if the given Ethernet packet belongs to our own interface's ip.
    ///
    /// # Arguments
    /// * `eth_packet` - The Ethernet packet to check.
    /// * `src_ips` - A list of source IP addresses to check against.
    ///
    /// # Returns
    /// A boolean indicating whether the packet is from our own interface's ip.
    pub fn is_it_own_packet(
        eth_packet: &MutableEthernetPacket<'_>,
        src_ips: &Vec<IpNetwork>,
    ) -> bool {
        match eth_packet.get_ethertype() {
            EtherTypes::Ipv4 => {
                // Parse the IPv4 packet
                if let Some(ipv4_packet) = Ipv4Packet::new(eth_packet.payload()) {
                    let src_ip = ipv4_packet.get_source();
                    // let result = src_ips.iter().any(|ip| ip.contains(src_ip.into()));
                    let result = src_ips.iter().any(
                        |ip_net| matches!(ip_net, IpNetwork::V4(v4_net) if v4_net.ip() == src_ip),
                    );

                    debug!(
                        "Ip:{:?},result:{},src_ips:{:?}",
                        ipv4_packet.get_source(),
                        result,
                        src_ips
                    );
                    // Check if the source IP matches any in src_ips
                    return result;
                }
            }
            EtherTypes::Ipv6 => {
                // Parse the IPv6 packet
                if let Some(ipv6_packet) = Ipv6Packet::new(eth_packet.payload()) {
                    let src_ip = ipv6_packet.get_source();
                    let result = src_ips.iter().any(
                        |ip_net| matches!(ip_net, IpNetwork::V6(v6_net) if v6_net.ip() == src_ip),
                    );
                    // Check if the source IP matches any in src_ips
                    return result;
                }
            }
            _ => {}
        }

        // If the packet is not IPv4 or IPv6, return false
        false
    }
    /// Modifies the packet received from the external network to forward it to the internal network.
    ///
    /// # Arguments
    /// * `eth_packet` - The Ethernet packet to modify.
    /// * `src_mac` - The source MAC address.
    /// * `dest_mac` - The destination MAC address.
    /// * `dest_ip` - The destination IP address.
    ///
    /// # Returns
    /// A `bool` representing the whether modified eth_packet to be sent to the internal network.
    fn modify_ext_to_int_packet(
        eth_packet: &mut MutableEthernetPacket,
        src_mac: MacAddr,
        dest_mac: MacAddr,
        dest_ip: IpNetwork,
    ) -> bool {
        eth_packet.set_destination(dest_mac);
        eth_packet.set_source(src_mac);
        if eth_packet.get_ethertype() == EtherTypes::Ipv4 {
            // Parse the IPv4 packet
            if let Some(mut ipv4_packet) =
                MutableIpv4Packet::new(&mut eth_packet.packet_mut()[14..])
            {
                // Extract source and destination IPs before modifying the packet
                let src_ip = ipv4_packet.get_source();

                // Modify destination IP
                let IpAddr::V4(dest_ipv4) = dest_ip.ip() else {
                    error!("Not an IPv4 address");
                    return false;
                };
                ipv4_packet.set_destination(dest_ipv4);

                if ipv4_packet.get_destination().is_multicast() {
                    ipv4_packet.set_ttl(1);
                }

                match ipv4_packet.get_next_level_protocol() {
                    IpNextHeaderProtocols::Tcp => {
                        if let Some(mut tcp_packet) =
                            MutableTcpPacket::new(ipv4_packet.payload_mut())
                        {
                            // Recalculate TCP checksum
                            let checksum =
                                tcp::ipv4_checksum(&tcp_packet.to_immutable(), &src_ip, &dest_ipv4);
                            tcp_packet.set_checksum(checksum);
                        }
                    }
                    IpNextHeaderProtocols::Udp => {
                        if let Some(mut udp_packet) =
                            MutableUdpPacket::new(ipv4_packet.payload_mut())
                        {
                            // Recalculate UDP checksum
                            udp_packet.set_checksum(0);

                            let checksum =
                                udp::ipv4_checksum(&udp_packet.to_immutable(), &src_ip, &dest_ipv4);
                            udp_packet.set_checksum(checksum);
                        }
                    }

                    _ => return false,
                }
                // println!("ipv4_packet:{:?}", ipv4_packet.packet());

                // Recalculate IPv4 checksum
                ipv4_packet.set_checksum(0); // Clear existing checksum

                match calculate_ipv4_checksum(ipv4_packet.packet()) {
                    Ok(checksum) => {
                        ipv4_packet.set_checksum(checksum);
                        debug!(
                            "Ext to Int - ipv4_packet: {:?}, checksum:{:?}",
                            ipv4_packet, checksum
                        );
                    }
                    Err(e) => {
                        error!("{}", e);
                        return false;
                    }
                }
            }
        } else {
            trace!("Ext to Int- it is not ipv4");
            return false;
        }

        trace!("Ext to Int-modified_packet:{:?}", eth_packet);

        true
    }

    fn calculate_ipv4_checksum(header: &[u8]) -> Result<u16, Box<dyn Error>> {
        if header.len() < 20 {
            return Err("IPv4 header must be at least 20 bytes long!".into());
        }

        // Only process the first 20 bytes (IPv4 header length)
        let header = &header[0..20];

        let mut sum: u32 = 0;

        // Iterate over 16-bit words
        for chunk in header.chunks(2) {
            // Convert two bytes into a single 16-bit word
            let word = u16::from_be_bytes([chunk[0], chunk[1]]);
            sum += word as u32;
        }

        // Add carries from the high 16 bits to the low 16 bits
        while (sum >> 16) > 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }

        // One's complement of the result
        let checksum = !(sum as u16);
        Ok(checksum)
    }

    /// Parses packet details and returns them as a string.
    pub fn parse_packet(eth_packet: &MutableEthernetPacket<'_>) -> String {
        // Extract source and destination MAC addresses
        let src_mac = eth_packet.get_source();
        let dest_mac = eth_packet.get_destination();
        // Parse the Ethernet frame
        match eth_packet.get_ethertype() {
            EtherTypes::Ipv4 => {
                // IPv4 packet handling
                if let Some(ipv4_packet) = Ipv4Packet::new(eth_packet.payload()) {
                    let src_ip = ipv4_packet.get_source();
                    let dest_ip = ipv4_packet.get_destination();
                    let protocol = ipv4_packet.get_next_level_protocol();

                    match protocol {
                        IpNextHeaderProtocols::Tcp => {
                            if let Some(tcp_packet) = TcpPacket::new(ipv4_packet.payload()) {
                                let src_port = tcp_packet.get_source();
                                let dest_port = tcp_packet.get_destination();
                                return format!(
                                "TCP Packet - Src IP: {}, Src Port: {}, Dest IP: {}, Dest Port: {}",
                                src_ip, src_port, dest_ip, dest_port
                            );
                            }
                        }
                        IpNextHeaderProtocols::Udp => {
                            if let Some(udp_packet) = UdpPacket::new(ipv4_packet.payload()) {
                                let src_port = udp_packet.get_source();
                                let dest_port = udp_packet.get_destination();
                                return format!(
                                "UDP Packet - Src MAC: {}, Src IP: {}, Src Port: {}, Dest MAC: {}, Dest IP: {}, Dest Port: {}",
                                src_mac,src_ip, src_port, dest_mac,dest_ip, dest_port
                            );
                            }
                        }
                        IpNextHeaderProtocols::Icmp => {
                            if let Some(icmp_packet) = IcmpPacket::new(ipv4_packet.payload()) {
                                return format!(
                                    "ICMP Packet - Src IP: {}, Dest IP: {}, Code: {:?}",
                                    src_ip,
                                    dest_ip,
                                    icmp_packet.get_icmp_type()
                                );
                            }
                        }
                        _ => {
                            return format!(
                                "Unknown IPv4 Protocol - Src IP: {}, Dest IP: {}, Protocol: {:?}",
                                src_ip, dest_ip, protocol
                            );
                        }
                    }
                } else {
                    return "Failed to parse IPv4 packet.".to_string();
                }
            }
            EtherTypes::Arp => {
                // ARP packet handling
                if let Some(arp_packet) = ArpPacket::new(eth_packet.payload()) {
                    return format!(
                        "ARP Packet - Sender IP: {}, Sender MAC: {}, Target IP: {}, Target MAC: {}",
                        arp_packet.get_sender_proto_addr(),
                        arp_packet.get_sender_hw_addr(),
                        arp_packet.get_target_proto_addr(),
                        arp_packet.get_target_hw_addr()
                    );
                }
            }
            _ => {
                return format!(
                    "Unknown Ethernet Frame - Ethertype: {:?}",
                    eth_packet.get_ethertype()
                );
            }
        }

        "Unable to determine packet details.".to_string()
    }

    /// Masquerading an Ethernet packet from an internal interface to an external one.
    ///
    /// This function checks if the packet should be forwarded based on several conditions:
    /// - If it's an IPv6 packet or the packet is not from the internal network, it's dropped.
    /// - If the packet is safe, it is modified by changing the source MAC and IP to the external interface's MAC and IP, and checksums are recalculated.
    ///
    /// # Arguments
    ///
    /// * `tx` - An `Arc<Mutex<Box<dyn pnet::datalink::DataLinkSender>>>` used to send the modified packet to the external interface.
    /// * `eth_packet` - A reference to an `EthernetPacket` which represents the packet to be forwarded.
    /// * `ifaces` - A reference to the `Ifaces` struct containing the network interfaces' details, including external IP and MAC addresses.
    pub async fn internal_to_external_process_packet(
        tx: &Arc<Mutex<Box<dyn pnet::datalink::DataLinkSender>>>,
        eth_packet: &mut MutableEthernetPacket<'_>,
        ifaces: &Ifaces,
    ) {
        let mut tx = tx.lock().await; // Acquire lock asynchronously
        let ext_mac = ifaces.ext_mac;
        let ext_ip = ifaces.ext_ip;
        let internal_ip = ifaces.int_ip;
        let is_ipv6: bool = eth_packet.get_ethertype() == EtherTypes::Ipv6;

        /*
        1) src_ip -> should be external ip
        2) dest_ip,dest mac -> leave as it is
        3) calculate crc and checksums again
        */
        if is_ipv6
            || !is_it_external_packet(eth_packet, &internal_ip)
            || !is_packet_safe(eth_packet)
        {
            debug!("Int to Ext - packet dropped {}", parse_packet(eth_packet));
        } else if modify_int_to_ext_packet(eth_packet, &ext_mac, &ext_ip) {
            match tx.send_to(eth_packet.packet(), None) {
                Some(Ok(_)) => {
                    info!(
                        "Int to Ext - Forwarded packet: {}",
                        parse_packet(eth_packet)
                    );
                    trace!("Int to ext - Forwarded packet(raw): {:?}", eth_packet);
                }
                Some(Err(e)) => {
                    error!("Int to Ext - Error sending packet: {}", e);
                }
                None => error!("Int to Ext - Send failed, no destination address."),
            }
        }
    }
    /// Checks whether the given Ethernet packet should be propagated to external network
    ///
    /// This function checks the destination IP address in the packet to determine if it belongs to the internal network.
    /// If the destination IP is not within the internal network, the packet is considered external.
    ///
    /// # Arguments
    ///
    /// * `eth_packet` - The `EthernetPacket` that needs to be checked.
    /// * `internal_ip` - The internal IP address (network) of the interface.
    ///
    /// # Returns
    ///
    /// `true` if the packet is external (should be forwarded), otherwise `false`.
    fn is_it_external_packet(
        eth_packet: &MutableEthernetPacket<'_>,
        internal_ip: &IpNetwork,
    ) -> bool {
        match eth_packet.get_ethertype() {
            EtherTypes::Ipv4 => {
                if let Some(ipv4_packet) = Ipv4Packet::new(eth_packet.payload()) {
                    let dest_ip = ipv4_packet.get_destination();
                    let src_ip = ipv4_packet.get_source();
                    // Check if the destination IP is in the same network as our_ip
                    return !internal_ip.contains(dest_ip.into())
                        || dest_ip.is_broadcast()
                        || (dest_ip.is_multicast() && internal_ip.contains(src_ip.into()));
                }
            }
            EtherTypes::Ipv6 => {
                if let Some(ipv6_packet) = Ipv6Packet::new(eth_packet.payload()) {
                    let dest_ip = ipv6_packet.get_destination();

                    // Check if the destination IP is in the same network as our_ip
                    return !internal_ip.contains(dest_ip.into()) || dest_ip.is_multicast();
                }
            }
            _ => {}
        }

        // If the packet is not IPv4 or IPv6, return false
        false
    }
    /// Modifies the Ethernet packet to forward it to the external network.
    ///
    /// This function modifies the packet's source MAC and IP addresses and recalculates
    /// the checksums for IPv4 and transport layer (TCP/UDP).
    ///
    /// # Arguments
    ///
    /// * `eth_packet` - A reference to the `MutableEthernetPacket` that needs to be modified.
    /// * `ext_iface_mac` - The MAC address of the external interface.
    /// * `ext_iface_ip` - The IP address of the external interface.
    ///
    /// # Returns
    /// A `bool` representing the whether modified eth_packet to be sent to the external network.
    fn modify_int_to_ext_packet(
        eth_packet: &mut MutableEthernetPacket,
        ext_iface_mac: &MacAddr,
        ext_iface_ip: &IpNetwork,
    ) -> bool {
        eth_packet.set_source(*ext_iface_mac);

        if eth_packet.get_ethertype() == EtherTypes::Ipv4 {
            // Parse the IPv4 packet
            if let Some(mut ipv4_packet) =
                MutableIpv4Packet::new(&mut eth_packet.packet_mut()[14..])
            {
                // Modify source IP
                let IpNetwork::V4(ipv4) = ext_iface_ip else {
                    error!("Not an IPv4 address");
                    return false;
                };
                ipv4_packet.set_source(ipv4.ip());

                let src_ip = ipv4_packet.get_source();
                let dest_ip = ipv4_packet.get_destination();

                match ipv4_packet.get_next_level_protocol() {
                    IpNextHeaderProtocols::Tcp => {
                        if let Some(mut tcp_packet) =
                            MutableTcpPacket::new(ipv4_packet.payload_mut())
                        {
                            // Recalculate TCP checksum
                            let checksum =
                                tcp::ipv4_checksum(&tcp_packet.to_immutable(), &src_ip, &dest_ip);
                            tcp_packet.set_checksum(checksum);
                        }
                    }
                    IpNextHeaderProtocols::Udp => {
                        if let Some(mut udp_packet) =
                            MutableUdpPacket::new(ipv4_packet.payload_mut())
                        {
                            udp_packet.set_checksum(0);

                            // Recalculate UDP checksum
                            let checksum =
                                udp::ipv4_checksum(&udp_packet.to_immutable(), &src_ip, &dest_ip);
                            udp_packet.set_checksum(checksum);
                        }
                    }

                    _ => return false,
                }
                // Recalculate IPv4 checksum
                ipv4_packet.set_checksum(0); // Clear existing checksum

                match calculate_ipv4_checksum(ipv4_packet.packet()) {
                    Ok(checksum) => {
                        ipv4_packet.set_checksum(checksum);
                        debug!(
                            "Int to Ext - ipv4_packet: {:?}, checksum:{:?}",
                            ipv4_packet, checksum
                        );
                    }
                    Err(e) => {
                        error!("{}", e);
                        return false;
                    }
                }
            }
        } else {
            trace!("Int to Ext- it is not ipv4");
            return false;
        }
        trace!("Int to Ext-modified_packet:{:?}", eth_packet);

        true
    }

    /// Checks if the packet is safe to forward.
    ///
    /// Currently, the safety checks are not implemented but should include checks like:
    /// - Loopback prevention
    /// - Rate limiting checks
    ///
    /// # Arguments
    ///
    /// * `eth_packet` - The Ethernet packet to be checked.
    ///
    /// # Returns
    ///
    fn is_packet_safe(_eth_packet: &MutableEthernetPacket<'_>) -> bool {
        //loopback check should be here
        //rate limiting should be here

        true
    }

    // A helper function that is only available in the test module
    #[cfg(test)]
    pub fn select_ip_test(
        iface: &NetworkInterface,
        iface_ip: Option<IpNetwork>,
    ) -> Result<IpNetwork, String> {
        select_ip(iface, iface_ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward;
    use pnet::datalink::Channel::Ethernet;
    use pnet::datalink::Config;
    use pnet::packet::ethernet::{EthernetPacket, MutableEthernetPacket};
    use pnet::packet::ipv4::MutableIpv4Packet;
    use pnet::packet::ipv6::MutableIpv6Packet;
    use std::net::{IpAddr, Ipv4Addr}; // Import the `forward` module

    #[test]
    fn test_is_it_own_packet_ipv4() {
        let src_ip = Ipv4Addr::new(192, 168, 1, 1);
        let src_ips = vec![IpNetwork::V4("192.168.1.1/24".parse().unwrap())];

        let mut ethernet_buffer = [0u8; 42];
        let mut ethernet_packet = MutableEthernetPacket::new(&mut ethernet_buffer).unwrap();
        ethernet_packet.set_ethertype(EtherTypes::Ipv4);

        let mut ipv4_buffer = [0u8; 28];
        let mut ipv4_packet: MutableIpv4Packet<'_> =
            MutableIpv4Packet::new(&mut ipv4_buffer).unwrap();
        ipv4_packet.set_source(src_ip);
        ipv4_packet.set_destination(Ipv4Addr::new(192, 168, 1, 2));
        ipv4_packet.set_next_level_protocol(IpNextHeaderProtocols::Udp);

        ethernet_packet.set_payload(ipv4_packet.packet());

        let eth_packet = MutableEthernetPacket::new(ethernet_packet.packet_mut()).unwrap();
        assert!(forward::is_it_own_packet(&eth_packet, &src_ips));
    }

    #[test]
    fn test_is_it_own_packet_ipv6() {
        let src_ip = "fe80::1".parse().unwrap();
        let src_ips = vec![IpNetwork::V6("fe80::1/64".parse().unwrap())];

        let mut ethernet_buffer = [0u8; 62];
        let mut ethernet_packet = MutableEthernetPacket::new(&mut ethernet_buffer).unwrap();
        ethernet_packet.set_ethertype(EtherTypes::Ipv6);

        let mut ipv6_buffer = [0u8; 48];
        let mut ipv6_packet = MutableIpv6Packet::new(&mut ipv6_buffer).unwrap();
        ipv6_packet.set_source(src_ip);
        ipv6_packet.set_destination("fe80::2".parse().unwrap());
        ipv6_packet.set_next_header(IpNextHeaderProtocols::Udp);

        ethernet_packet.set_payload(ipv6_packet.packet());

        let eth_packet = MutableEthernetPacket::new(ethernet_packet.packet_mut()).unwrap();
        assert!(forward::is_it_own_packet(&eth_packet, &src_ips));
    }

    #[test]
    fn test_select_ip_with_single_ipv4() {
        let iface = NetworkInterface {
            name: "eth0".to_string(),
            description: "Ethernet interface".to_string(),
            index: 0,
            mac: None,
            ips: vec![IpNetwork::V4("192.168.1.1/24".parse().unwrap())],
            flags: 0,
        };
        let iface_ip = None;
        let result = forward::select_ip_test(&iface, iface_ip);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            IpNetwork::V4("192.168.1.1/24".parse().unwrap())
        );
    }

    #[test]
    fn test_select_ip_with_multiple_ipv4() {
        let iface = NetworkInterface {
            name: "eth0".to_string(),
            description: "Ethernet interface".to_string(),
            index: 0,
            mac: None,
            ips: vec![
                IpNetwork::V4("192.168.1.1/24".parse().unwrap()),
                IpNetwork::V4("192.168.1.2/24".parse().unwrap()),
            ],
            flags: 0,
        };
        let iface_ip = Some(IpNetwork::V4("192.168.1.1/24".parse().unwrap()));
        let result = forward::select_ip_test(&iface, iface_ip);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            IpNetwork::V4("192.168.1.1/24".parse().unwrap())
        );
    }

    #[test]
    fn test_select_ip_with_no_ipv4() {
        let iface = NetworkInterface {
            name: "eth0".to_string(),
            description: "Ethernet interface".to_string(),
            index: 0,
            mac: None,
            ips: vec![IpNetwork::V6("fe80::1/64".parse().unwrap())],
            flags: 0,
        };
        let result = forward::select_ip_test(&iface, None);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "No IPv4 address found for interface eth0".to_string()
        );
    }

    #[test]
    fn test_select_ip_with_non_matching_iface_ip() {
        let iface = NetworkInterface {
            name: "eth0".to_string(),
            description: "Ethernet interface".to_string(),
            index: 0,
            mac: None,
            ips: vec![
                IpNetwork::V4("192.168.1.1/24".parse().unwrap()),
                IpNetwork::V4("192.168.1.2/24".parse().unwrap()),
            ],
            flags: 0,
        };
        let iface_ip = Some(IpNetwork::V4("192.168.1.3/24".parse().unwrap()));
        let result = forward::select_ip_test(&iface, iface_ip);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Provided IP 192.168.1.3/24 does not match any IPs in interface eth0"
        );
    }
}
