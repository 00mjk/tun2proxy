use smoltcp::wire::Ipv4Cidr;
use std::collections::{HashMap, LinkedList};
use std::convert::{TryFrom, TryInto};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::time::{Duration, Instant};

const DNS_TTL: u8 = 30; // TTL in DNS replies
const MAPPING_TIMEOUT: u64 = 60; // Mapping timeout

#[derive(Eq, PartialEq, Debug)]
#[allow(dead_code, clippy::upper_case_acronyms)]
enum DnsRecordType {
    A = 1,
    AAAA = 28,
}

#[derive(Eq, PartialEq, Debug)]
#[allow(dead_code)]
enum DnsClass {
    IN = 1,
}

#[derive(Clone, Debug)]
pub struct VirtualDns {
    ip_to_name: HashMap<IpAddr, String>,
    expiry: LinkedList<(IpAddr, Instant)>,
    name_to_ip: HashMap<String, IpAddr>,
    network_addr: IpAddr,
    broadcast_addr: IpAddr,
    next_addr: IpAddr,
}

impl Default for VirtualDns {
    fn default() -> Self {
        let start_addr = Ipv4Addr::from_str("198.18.0.0").unwrap();
        let cidr = Ipv4Cidr::new(start_addr.into(), 15);

        Self {
            next_addr: start_addr.into(),
            ip_to_name: Default::default(),
            name_to_ip: Default::default(),
            expiry: Default::default(),
            network_addr: IpAddr::try_from(cidr.network().address().into_address()).unwrap(),
            broadcast_addr: IpAddr::try_from(cidr.broadcast().unwrap().into_address()).unwrap(),
        }
    }
}

impl VirtualDns {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn receive_query(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 17 {
            return None;
        }
        // bit 1: Message is a query (0)
        // bits 2 - 5: Standard query opcode (0)
        // bit 6: Unused
        // bit 7: Message is not truncated (0)
        // bit 8: Recursion desired (1)
        let is_supported_query = (data[2] & 0b11111011) == 0b00000001;
        let num_queries = (data[4] as u16) << 8 | data[5] as u16;
        if !is_supported_query || num_queries != 1 {
            return None;
        }

        let result = VirtualDns::parse_qname(data, 12);
        let (qname, offset) = result?;
        if offset + 3 >= data.len() {
            return None;
        }
        let qtype = (data[offset] as u16) << 8 | data[offset + 1] as u16;
        let qclass = (data[offset + 2] as u16) << 8 | data[offset + 3] as u16;

        if qtype != DnsRecordType::A as u16 && qtype != DnsRecordType::AAAA as u16
            || qclass != DnsClass::IN as u16
        {
            return None;
        }

        log::info!("DNS query: {}", qname);

        let mut response = Vec::<u8>::new();
        response.extend(&data[0..offset + 4]);
        response[2] |= 0x80; // Message is a response
        response[3] |= 0x80; // Recursion available

        // Record count of the answer section:
        // We only send an answer record for A queries, assuming that IPv4 is supported everywhere.
        // This way, we do not have to handle two IP spaces for the virtual DNS feature.
        response[6] = 0;
        response[7] = if qtype == DnsRecordType::A as u16 {
            1
        } else {
            0
        };

        // Zero count of other sections:
        // authority section
        response[8] = 0;
        response[9] = 0;

        // additional section
        response[10] = 0;
        response[11] = 0;
        if qtype == DnsRecordType::A as u16 {
            if let Some(ip) = self.allocate_ip(qname) {
                response.extend(&[
                    0xc0, 0x0c, // Question name pointer
                    0, 1, // Record type: A
                    0, 1, // Class: IN
                    0, 0, 0, DNS_TTL, // TTL
                    0, 4, // Data length: 4 bytes
                ]);
                match ip as IpAddr {
                    IpAddr::V4(ip) => response.extend(ip.octets().as_ref()),
                    IpAddr::V6(ip) => response.extend(ip.octets().as_ref()),
                };
            } else {
                log::error!("Virtual IP space for DNS exhausted");
                response[7] = 0; // No answers

                // Set rcode to SERVFAIL
                response[3] &= 0xf0;
                response[3] |= 2;
            }
        } else {
            response[7] = 0; // No answers
        }
        Some(response)
    }

    fn increment_ip(addr: IpAddr) -> IpAddr {
        let mut ip_bytes = match addr as IpAddr {
            IpAddr::V4(ip) => Vec::<u8>::from(ip.octets()),
            IpAddr::V6(ip) => Vec::<u8>::from(ip.octets()),
        };

        // Traverse bytes from right to left and stop when we can add one.
        for j in 0..ip_bytes.len() {
            let i = ip_bytes.len() - 1 - j;
            if ip_bytes[i] != 255 {
                // We can add 1 without carry and are done.
                ip_bytes[i] += 1;
                break;
            } else {
                // Zero this byte and carry over to the next one.
                ip_bytes[i] = 0;
            }
        }
        if addr.is_ipv4() {
            let bytes: [u8; 4] = ip_bytes.as_slice().try_into().unwrap();
            IpAddr::V4(Ipv4Addr::from(bytes))
        } else {
            let bytes: [u8; 16] = ip_bytes.as_slice().try_into().unwrap();
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
    }

    pub fn ip_to_name(&self, addr: &IpAddr) -> Option<&String> {
        self.ip_to_name.get(addr)
    }

    fn allocate_ip(&mut self, name: String) -> Option<IpAddr> {
        let now = Instant::now();
        while let Some((ip, expiry)) = self.expiry.front() {
            if now > *expiry {
                let name = self.ip_to_name.remove(ip).unwrap();
                self.name_to_ip.remove(&name);
                self.expiry.pop_front();
            } else {
                break;
            }
        }

        if let Some(ip) = self.name_to_ip.get(&name) {
            return Some(*ip);
        }

        let started_at = self.next_addr;

        loop {
            if let std::collections::hash_map::Entry::Vacant(e) =
                self.ip_to_name.entry(self.next_addr)
            {
                e.insert(name.clone());
                self.name_to_ip.insert(name, self.next_addr);
                self.expiry.push_back((
                    self.next_addr,
                    Instant::now() + Duration::from_secs(MAPPING_TIMEOUT),
                ));
                return Some(self.next_addr);
            }
            self.next_addr = Self::increment_ip(self.next_addr);
            if self.next_addr == self.broadcast_addr {
                // Wrap around.
                self.next_addr = self.network_addr;
            }
            if self.next_addr == started_at {
                return None;
            }
        }
    }

    /// Parse a DNS qname at a specific offset and return the name along with its size.
    /// DNS packet parsing should be continued after the name.
    fn parse_qname(data: &[u8], mut offset: usize) -> Option<(String, usize)> {
        // Since we only parse qnames and qnames can't point anywhere,
        // we do not support pointers. (0xC0 is a bitmask for pointer detection.)
        let label_type = data[offset] & 0xC0;
        if label_type != 0x00 {
            return None;
        }

        let mut qname = String::from("");
        loop {
            if offset >= data.len() {
                return None;
            }
            let label_len = data[offset];
            if label_len == 0 {
                offset += 1;
                break;
            }
            for _ in 0..label_len {
                offset += 1;
                if offset >= data.len() {
                    return None;
                }
                qname.push(data[offset] as char);
            }
            qname.push('.');
            offset += 1;
        }

        Some((qname, offset))
    }
}
