//! 进程面板 + 磁盘/网络 I/O(读 /proc,增量计算 CPU% 与吞吐)。
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::time::Instant;

const CLK_TCK: f64 = 100.0; // Linux 默认
const SECTOR: f64 = 512.0;

#[derive(Clone)]
pub struct ProcInfo {
    pub pid: i32,
    pub user: String,
    pub cpu_pct: f64,
    pub rss_kb: u64,
    pub mem_pct: f64,
    pub time_secs: u64,
    pub threads: u64,
    pub devices: String,
    pub cmd: String,
}

#[derive(Default, Clone)]
pub struct IoStats {
    pub disk_dev: String,
    pub disk_r: f64, // MB/s
    pub disk_w: f64,
    pub net_rx: f64,
    pub net_tx: f64,
}

pub struct SysMon {
    users: HashMap<u32, String>,
    mem_total_kb: u64,
    disk_dev: String,
    prev_cpu: HashMap<i32, u64>,
    prev_instant: Option<Instant>,
    prev_disk: Option<(u64, u64)>,
    prev_net: Option<(u64, u64)>,
}

fn read_users() -> HashMap<u32, String> {
    let mut m = HashMap::new();
    if let Ok(txt) = fs::read_to_string("/etc/passwd") {
        for line in txt.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() >= 3 {
                if let Ok(uid) = f[2].parse::<u32>() {
                    m.insert(uid, f[0].to_string());
                }
            }
        }
    }
    m
}

fn mem_total_kb() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

/// 找出挂载点对应的块设备名(如 nvme0n2)。
fn disk_device_for(mount: &str) -> String {
    if let Ok(txt) = fs::read_to_string("/proc/mounts") {
        for line in txt.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 2 && f[1] == mount {
                return f[0].rsplit('/').next().unwrap_or("").to_string();
            }
        }
    }
    String::new()
}

/// 把设备号列表压成紧凑串:0,1,2,3 -> "0-3"。
fn compress(mut ids: Vec<i64>) -> String {
    ids.sort_unstable();
    ids.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let start = ids[i];
        let mut end = start;
        while i + 1 < ids.len() && ids[i + 1] == end + 1 {
            end = ids[i + 1];
            i += 1;
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}-{end}"));
        }
        i += 1;
    }
    parts.join(",")
}

impl SysMon {
    pub fn new(mount: &str) -> Self {
        Self {
            users: read_users(),
            mem_total_kb: mem_total_kb(),
            disk_dev: disk_device_for(mount),
            prev_cpu: HashMap::new(),
            prev_instant: None,
            prev_disk: None,
            prev_net: None,
        }
    }

    fn read_proc(&self, pid: i32, dt: f64, devices: &str) -> Option<(ProcInfo, u64)> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // comm 可能含空格,取最后一个 ')' 之后
        let rest = &stat[stat.rfind(')')? + 1..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        // rest[0]=state(field3);utime=f14->rest[11];stime=f15->rest[12];threads=f20->rest[17]
        let utime: u64 = f.get(11)?.parse().ok()?;
        let stime: u64 = f.get(12)?.parse().ok()?;
        let threads: u64 = f.get(17).and_then(|v| v.parse().ok()).unwrap_or(0);
        let total = utime + stime;

        let cpu_pct = match self.prev_cpu.get(&pid) {
            Some(&prev) if dt > 0.0 => (total.saturating_sub(prev) as f64 / CLK_TCK) / dt * 100.0,
            _ => 0.0,
        };

        let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let rss_kb = status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let uid = status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let user = self.users.get(&uid).cloned().unwrap_or_else(|| uid.to_string());

        let cmd = fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|raw| {
                raw.split(|&b| b == 0)
                    .filter(|c| !c.is_empty())
                    .map(|c| String::from_utf8_lossy(c).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("[{}]", stat.split('(').nth(1).and_then(|s| s.split(')').next()).unwrap_or("?")));

        let mem_pct = if self.mem_total_kb > 0 {
            rss_kb as f64 / self.mem_total_kb as f64 * 100.0
        } else {
            0.0
        };

        Some((
            ProcInfo {
                pid,
                user,
                cpu_pct,
                rss_kb,
                mem_pct,
                time_secs: (total as f64 / CLK_TCK) as u64,
                threads,
                devices: devices.to_string(),
                cmd,
            },
            total,
        ))
    }

    fn read_disk(&self) -> Option<(u64, u64)> {
        if self.disk_dev.is_empty() {
            return None;
        }
        let txt = fs::read_to_string("/proc/diskstats").ok()?;
        for line in txt.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 10 && f[2] == self.disk_dev {
                let r: u64 = f[5].parse().ok()?; // sectors read
                let w: u64 = f[9].parse().ok()?; // sectors written
                return Some((r, w));
            }
        }
        None
    }

    fn read_net(&self) -> Option<(u64, u64)> {
        let txt = fs::read_to_string("/proc/net/dev").ok()?;
        let mut rx = 0u64;
        let mut tx = 0u64;
        for line in txt.lines() {
            if let Some((iface, rest)) = line.split_once(':') {
                let iface = iface.trim();
                if iface == "lo" {
                    continue;
                }
                let f: Vec<&str> = rest.split_whitespace().collect();
                if f.len() >= 9 {
                    rx += f[0].parse::<u64>().unwrap_or(0);
                    tx += f[8].parse::<u64>().unwrap_or(0);
                }
            }
        }
        Some((rx, tx))
    }

    /// 采集一轮。owners: {device_id -> pid}。返回按 CPU% 降序的进程列表 + I/O。
    pub fn sample(&mut self, owners: &BTreeMap<i64, i32>) -> (Vec<ProcInfo>, IoStats) {
        let now = Instant::now();
        let dt = self
            .prev_instant
            .map(|p| now.duration_since(p).as_secs_f64())
            .unwrap_or(0.0);

        // pid -> 持有的 device_id
        let mut pid_devs: HashMap<i32, Vec<i64>> = HashMap::new();
        for (dev, pid) in owners {
            pid_devs.entry(*pid).or_default().push(*dev);
        }

        let mut procs = Vec::new();
        let mut new_cpu = HashMap::new();
        for (pid, devs) in &pid_devs {
            let dev_str = compress(devs.clone());
            if let Some((info, total)) = self.read_proc(*pid, dt, &dev_str) {
                new_cpu.insert(*pid, total);
                procs.push(info);
            }
        }
        procs.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));
        self.prev_cpu = new_cpu;

        // I/O 增量
        let mut io = IoStats {
            disk_dev: self.disk_dev.clone(),
            ..Default::default()
        };
        if let Some((r, w)) = self.read_disk() {
            if let (Some((pr, pw)), true) = (self.prev_disk, dt > 0.0) {
                io.disk_r = (r.saturating_sub(pr) as f64 * SECTOR) / 1e6 / dt;
                io.disk_w = (w.saturating_sub(pw) as f64 * SECTOR) / 1e6 / dt;
            }
            self.prev_disk = Some((r, w));
        }
        if let Some((rx, tx)) = self.read_net() {
            if let (Some((prx, ptx)), true) = (self.prev_net, dt > 0.0) {
                io.net_rx = (rx.saturating_sub(prx) as f64) / 1e6 / dt;
                io.net_tx = (tx.saturating_sub(ptx) as f64) / 1e6 / dt;
            }
            self.prev_net = Some((rx, tx));
        }

        self.prev_instant = Some(now);
        (procs, io)
    }
}
