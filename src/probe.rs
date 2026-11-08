use crate::model::valid_ipv4;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const ARP_PATH: &str = "/proc/net/arp";
pub const DNSMASQ_LEASE_GLOBS: &[&str] = &[
    "/var/lib/misc/dnsmasq.*.leases",
    "/var/lib/dnsmasq/*.leases",
];

/// mac (lowercase) -> IPv4s from /proc/net/arp, skipping incomplete (flags 0x0) rows.
pub fn read_arp_table(path: &str) -> HashMap<String, Vec<String>> {
    let mut res: HashMap<String, Vec<String>> = HashMap::new();
    let p = Path::new(path);
    if !p.is_file() {
        return res;
    }
    let content = match std::fs::read_to_string(p) {
        Ok(c) => c,
        Err(_) => return res,
    };
    for line in content.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }
        let (ip_raw, flags_raw, mac_raw) = (parts[0], parts[2], parts[3]);
        if flags_raw == "0x0" {
            continue;
        }
        let v_ip = valid_ipv4(ip_raw);
        let mac = mac_raw.to_ascii_lowercase();
        if !v_ip.is_empty() && !mac.is_empty() && mac != "00:00:00:00:00:00" {
            let entry = res.entry(mac).or_default();
            if !entry.contains(&v_ip) {
                entry.push(v_ip);
            }
        }
    }
    res
}

/// mac -> IPv4s from dnsmasq leases: 'expiry mac ip hostname clientid'.
pub fn read_dhcp_leases(globs: &[&str]) -> HashMap<String, Vec<String>> {
    let mut mac_to_ips: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen_files: HashSet<PathBuf> = HashSet::new();

    for &pattern in globs {
        let paths = match glob::glob(pattern) {
            Ok(paths) => paths,
            Err(_) => continue,
        };
        for entry in paths {
            let filepath = match entry {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !seen_files.insert(filepath.clone()) {
                continue;
            }
            let content = match std::fs::read_to_string(&filepath) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "[warn] Failed to read lease file {}: {}",
                        filepath.display(),
                        e
                    );
                    continue;
                }
            };
            for line in content.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 3 {
                    continue;
                }
                let (mac_raw, ip_raw) = (parts[1], parts[2]);
                let mac = mac_raw.to_ascii_lowercase();
                let v_ip = valid_ipv4(ip_raw);
                if !v_ip.is_empty() && !mac.is_empty() {
                    let entry = mac_to_ips.entry(mac.clone()).or_default();
                    if !entry.contains(&v_ip) {
                        entry.push(v_ip);
                    }
                }
            }
        }
    }

    mac_to_ips
}
