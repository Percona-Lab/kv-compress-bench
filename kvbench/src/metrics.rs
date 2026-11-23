//! System / server metrics sampled once per interval.

use crate::resp;
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub struct InfoConn {
    s: Option<TcpStream>,
    addr: SocketAddr,
}

impl InfoConn {
    pub fn new(addr: SocketAddr) -> Self {
        InfoConn { s: None, addr }
    }

    /// Send a command and return the raw reply text (bulk/simple), or None.
    pub fn cmd(&mut self, args: &[&[u8]]) -> Option<String> {
        if self.s.is_none() {
            let s = TcpStream::connect_timeout(&self.addr, Duration::from_secs(2)).ok()?;
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            s.set_nodelay(true).ok();
            self.s = Some(s);
        }
        let s = self.s.as_mut().unwrap();
        let mut out = Vec::new();
        resp::encode_cmd(&mut out, args);
        if s.write_all(&out).is_err() {
            self.s = None;
            return None;
        }
        let mut buf = Vec::with_capacity(16384);
        let mut tmp = [0u8; 65536];
        loop {
            match resp::parse(&buf, 0) {
                Ok(Some((r, _))) => {
                    return match r {
                        resp::Reply::Bulk(st, l) => Some(String::from_utf8_lossy(&buf[st..st + l]).into_owned()),
                        _ => {
                            let end = buf.windows(2).position(|w| w == b"\r\n").unwrap_or(buf.len());
                            Some(String::from_utf8_lossy(&buf[..end]).into_owned())
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    self.s = None;
                    return None;
                }
            }
            match s.read(&mut tmp) {
                Ok(0) | Err(_) => {
                    self.s = None;
                    return None;
                }
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
    }

    /// INFO fields of interest (missing fields are skipped; stores differ).
    pub fn info(&mut self) -> Map<String, Value> {
        const KEEP: &[&str] = &[
            "used_memory", "used_memory_rss", "used_memory_peak", "mem_fragmentation_ratio",
            "used_cpu_user", "used_cpu_sys", "used_cpu_user_main_thread", "used_cpu_sys_main_thread",
            "evicted_keys", "keyspace_hits", "keyspace_misses", "connected_clients",
            "used_disk_percent", "used_db_size", "estimate_keys",
            "redis_version", "valkey_version", "kvrocks_version", "server_name", "version",
        ];
        let mut m = Map::new();
        if let Some(text) = self.cmd(&[b"INFO"]) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    let v = v.trim();
                    if KEEP.contains(&k) {
                        m.insert(k.to_string(), num_or_str(v));
                    } else if k.starts_with("db") && k[2..].chars().all(|c| c.is_ascii_digit()) {
                        // db0:keys=123,expires=0,...
                        for kv in v.split(',') {
                            if let Some(("keys", n)) = kv.split_once('=') {
                                m.insert(format!("{k}_keys"), num_or_str(n));
                            }
                        }
                    }
                }
            }
        }
        m
    }
}

fn num_or_str(v: &str) -> Value {
    if let Ok(i) = v.parse::<i64>() {
        json!(i)
    } else if let Ok(f) = v.parse::<f64>() {
        json!(f)
    } else {
        json!(v)
    }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn clk_tck() -> f64 {
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

/// Cumulative counters / gauges at one instant.
#[derive(Clone, Default)]
pub struct Sample {
    pub t_ns: u64,
    pub client_cpu_s: f64,
    pub server_cpu_s: Option<f64>,
    pub server_rss_kb: Option<u64>,
    pub server_swap_kb: Option<u64>,
    pub cpu_busy_s: f64,
    pub cpu_iowait_s: f64,
    pub cpu_total_s: f64,
    pub pswpin: u64,
    pub pswpout: u64,
    pub pgmajfault: u64,
    pub zswpin: u64,
    pub zswpout: u64,
    pub zswpwb: u64,
    pub server_majflt: Option<u64>,
    pub meminfo: Map<String, Value>,
    pub zram: Map<String, Value>,
    pub psi_mem_some_us: Option<u64>,
    pub psi_mem_full_us: Option<u64>,
    pub disk_read_sectors: u64,
    pub disk_write_sectors: u64,
}

pub fn client_cpu_s() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Sum utime+stime over all threads of `pid` (from /proc/pid/stat).
fn proc_cpu_s(pid: u32) -> Option<f64> {
    let s = read(&format!("/proc/{pid}/stat"))?;
    let after = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = after.split_whitespace().collect();
    let ut: f64 = f.get(11)?.parse().ok()?;
    let st: f64 = f.get(12)?.parse().ok()?;
    Some((ut + st) / clk_tck())
}

/// Major page faults of `pid` (all threads): for a swapping server each one is a page read back from swap
/// (zram: decompressed; zswap: decompressed from the pool or read from disk; disk swap: read from disk).
fn proc_majflt(pid: u32) -> Option<u64> {
    let s = read(&format!("/proc/{pid}/stat"))?;
    let after = &s[s.rfind(')')? + 2..];
    after.split_whitespace().nth(9)?.parse().ok()
}

pub fn sample(t_ns: u64, server_pid: Option<u32>) -> Sample {
    let mut s = Sample { t_ns, client_cpu_s: client_cpu_s(), ..Default::default() };
    if let Some(pid) = server_pid {
        s.server_cpu_s = proc_cpu_s(pid);
        s.server_majflt = proc_majflt(pid);
        if let Some(st) = read(&format!("/proc/{pid}/status")) {
            for l in st.lines() {
                let kb = || l.split_whitespace().nth(1).and_then(|x| x.parse().ok());
                if l.starts_with("VmRSS:") {
                    s.server_rss_kb = kb();
                } else if l.starts_with("VmSwap:") {
                    s.server_swap_kb = kb();
                }
            }
        }
    }
    if let Some(st) = read("/proc/stat") {
        if let Some(l) = st.lines().next() {
            let v: Vec<f64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let tck = clk_tck();
            let total: f64 = v.iter().take(8).sum();
            let idle = v.get(3).copied().unwrap_or(0.0);
            let iow = v.get(4).copied().unwrap_or(0.0);
            s.cpu_total_s = total / tck;
            s.cpu_busy_s = (total - idle - iow) / tck;
            s.cpu_iowait_s = iow / tck;
        }
    }
    if let Some(vm) = read("/proc/vmstat") {
        for l in vm.lines() {
            let mut it = l.split_whitespace();
            match (it.next(), it.next().and_then(|x| x.parse::<u64>().ok())) {
                (Some("pswpin"), Some(v)) => s.pswpin = v,
                (Some("pswpout"), Some(v)) => s.pswpout = v,
                (Some("pgmajfault"), Some(v)) => s.pgmajfault = v,
                (Some("zswpin"), Some(v)) => s.zswpin = v,
                (Some("zswpout"), Some(v)) => s.zswpout = v,
                (Some("zswpwb"), Some(v)) => s.zswpwb = v,
                _ => {}
            }
        }
    }
    if let Some(mi) = read("/proc/meminfo") {
        for l in mi.lines() {
            let mut it = l.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                let k = k.trim_end_matches(':');
                if ["MemTotal", "MemFree", "MemAvailable", "Cached", "SwapTotal", "SwapFree", "Zswap", "Zswapped", "AnonPages"].contains(&k) {
                    if let Ok(v) = v.parse::<u64>() {
                        s.meminfo.insert(format!("{k}_kb"), json!(v));
                    }
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir("/sys/block") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("zram") {
                if let Some(mm) = read(&format!("/sys/block/{name}/mm_stat")) {
                    let v: Vec<u64> = mm.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                    if v.len() >= 3 {
                        s.zram.insert(format!("{name}_orig_bytes"), json!(v[0]));
                        s.zram.insert(format!("{name}_compr_bytes"), json!(v[1]));
                        s.zram.insert(format!("{name}_mem_used_bytes"), json!(v[2]));
                    }
                }
            }
        }
    }
    if let Some(p) = read("/proc/pressure/memory") {
        for l in p.lines() {
            let tot = l.split_whitespace().find_map(|x| x.strip_prefix("total=")).and_then(|x| x.parse().ok());
            if l.starts_with("some") {
                s.psi_mem_some_us = tot;
            } else if l.starts_with("full") {
                s.psi_mem_full_us = tot;
            }
        }
    }
    if let Some(ds) = read("/proc/diskstats") {
        for l in ds.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() > 9 {
                let n = f[2];
                // whole physical disks only
                let whole = (n.starts_with("sd") && n.len() == 3) || (n.starts_with("nvme") && !n.contains('p'));
                if whole {
                    s.disk_read_sectors += f[5].parse::<u64>().unwrap_or(0);
                    s.disk_write_sectors += f[9].parse::<u64>().unwrap_or(0);
                }
            }
        }
    }
    s
}

/// Rates / deltas between two samples.
pub fn delta(a: &Sample, b: &Sample) -> Map<String, Value> {
    let dt = (b.t_ns - a.t_ns) as f64 / 1e9;
    let mut m = Map::new();
    m.insert("client_cpu_s".into(), json!(b.client_cpu_s - a.client_cpu_s));
    if let (Some(x), Some(y)) = (a.server_cpu_s, b.server_cpu_s) {
        m.insert("server_cpu_s".into(), json!(y - x));
    }
    if let Some(r) = b.server_rss_kb {
        m.insert("server_rss_kb".into(), json!(r));
    }
    if let Some(r) = b.server_swap_kb {
        m.insert("server_swap_kb".into(), json!(r));
    }
    let tot = b.cpu_total_s - a.cpu_total_s;
    if tot > 0.0 {
        m.insert("host_cpu_busy_pct".into(), json!(100.0 * (b.cpu_busy_s - a.cpu_busy_s) / tot));
        m.insert("host_cpu_iowait_pct".into(), json!(100.0 * (b.cpu_iowait_s - a.cpu_iowait_s) / tot));
    }
    m.insert("swapin_pages_s".into(), json!((b.pswpin - a.pswpin) as f64 / dt));
    m.insert("swapout_pages_s".into(), json!((b.pswpout - a.pswpout) as f64 / dt));
    m.insert("majfault_s".into(), json!((b.pgmajfault - a.pgmajfault) as f64 / dt));
    // zswap loads/stores are not counted in pswpin/pswpout
    m.insert("zswpin_s".into(), json!((b.zswpin - a.zswpin) as f64 / dt));
    m.insert("zswpout_s".into(), json!((b.zswpout - a.zswpout) as f64 / dt));
    m.insert("zswap_writeback_s".into(), json!((b.zswpwb - a.zswpwb) as f64 / dt));
    if let (Some(x), Some(y)) = (a.server_majflt, b.server_majflt) {
        m.insert("server_majflt_s".into(), json!((y - x) as f64 / dt));
        m.insert("server_majflt_total".into(), json!(y - x));
    }
    m.insert("disk_read_mb_s".into(), json!((b.disk_read_sectors - a.disk_read_sectors) as f64 * 512.0 / 1e6 / dt));
    m.insert("disk_write_mb_s".into(), json!((b.disk_write_sectors - a.disk_write_sectors) as f64 * 512.0 / 1e6 / dt));
    if let (Some(x), Some(y)) = (a.psi_mem_some_us, b.psi_mem_some_us) {
        m.insert("psi_mem_some_pct".into(), json!((y - x) as f64 / 1e4 / dt));
    }
    if let (Some(x), Some(y)) = (a.psi_mem_full_us, b.psi_mem_full_us) {
        m.insert("psi_mem_full_pct".into(), json!((y - x) as f64 / 1e4 / dt));
    }
    for (k, v) in &b.meminfo {
        m.insert(k.clone(), v.clone());
    }
    for (k, v) in &b.zram {
        m.insert(k.clone(), v.clone());
    }
    m
}
