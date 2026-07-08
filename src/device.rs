//! 静态 TPU 信息:纯读 /sys 和 /proc,不依赖任何运行时服务。
use std::collections::BTreeMap;
use std::fs;

#[derive(Clone, Debug)]
pub struct ChipType {
    pub name: &'static str,
    pub hbm_gib: u32,
    pub devices_per_chip: u32,
}

/// PCI device-id (+ subsystem) -> 芯片型号。映射来自 tpu_info.device。
fn chip_from_ids(device_id: &str, subsystem_id: &str) -> Option<ChipType> {
    match device_id {
        // v2 / v3 共用 device id,靠 subsystem 区分
        "0x0027" => match subsystem_id {
            "0x004e" => Some(ChipType { name: "v2", hbm_gib: 8, devices_per_chip: 2 }),
            "0x004f" => Some(ChipType { name: "v3", hbm_gib: 16, devices_per_chip: 2 }),
            _ => None,
        },
        "0x005e" => Some(ChipType { name: "v4", hbm_gib: 32, devices_per_chip: 1 }),
        "0x0063" => Some(ChipType { name: "v5e", hbm_gib: 16, devices_per_chip: 1 }),
        "0x0062" => Some(ChipType { name: "v5p", hbm_gib: 95, devices_per_chip: 1 }),
        "0x006f" => Some(ChipType { name: "v6e", hbm_gib: 32, devices_per_chip: 1 }),
        "0x0076" => Some(ChipType { name: "7x", hbm_gib: 192, devices_per_chip: 2 }),
        _ => None,
    }
}

pub struct DeviceInfo {
    pub chip: Option<ChipType>,
    pub chips: usize, // 物理芯片数(匹配 device-id 的 PCI function 数)
    pub cores: usize, // 逻辑 core 数 = chips × devices_per_chip(v7x 每芯片 2 core)
}

const GOOGLE_VENDOR: &str = "0x1ae0";

pub fn detect() -> DeviceInfo {
    let mut chip = None;
    let mut count = 0usize;
    if let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") {
        for e in entries.flatten() {
            let p = e.path();
            let vendor = fs::read_to_string(p.join("vendor")).unwrap_or_default();
            if vendor.trim() != GOOGLE_VENDOR {
                continue;
            }
            let device_id = fs::read_to_string(p.join("device")).unwrap_or_default();
            let subsystem = fs::read_to_string(p.join("subsystem_device")).unwrap_or_default();
            if let Some(c) = chip_from_ids(device_id.trim(), subsystem.trim()) {
                chip = Some(c);
                count += 1;
            }
        }
    }
    // PCI 匹配到的 function 数 = core 数(v6e 每芯片 1、v7x 每芯片 2 个 function/core)。
    // 物理芯片数 = cores / 每芯片 core 数。
    let dpc = chip.as_ref().map(|c| c.devices_per_chip as usize).unwrap_or(1).max(1);
    DeviceInfo {
        chip,
        chips: count / dpc,
        cores: count,
    }
}

/// 扫描 /proc/*/fd,找出打开了 /dev/vfio/N 或 /dev/accelN 的进程。
/// 返回 {设备路径 -> PID}。读取其他用户的 fd 需要 root,否则会被静默跳过。
pub fn chip_owners() -> BTreeMap<String, i32> {
    let mut owners = BTreeMap::new();
    let procs = match fs::read_dir("/proc") {
        Ok(p) => p,
        Err(_) => return owners,
    };
    for pe in procs.flatten() {
        let pid: i32 = match pe.file_name().to_string_lossy().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let fd_dir = pe.path().join("fd");
        if let Ok(fds) = fs::read_dir(&fd_dir) {
            for fd in fds.flatten() {
                if let Ok(target) = fs::read_link(fd.path()) {
                    let t = target.to_string_lossy().to_string();
                    let is_tpu = t.starts_with("/dev/vfio/") || t.starts_with("/dev/accel");
                    let ends_num = t
                        .rsplit('/')
                        .next()
                        .map(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                        .unwrap_or(false);
                    if is_tpu && ends_num {
                        owners.insert(t, pid);
                    }
                }
            }
        }
    }
    owners
}

/// 取进程命令行(用于展示),失败返回 None。
pub fn cmdline(pid: i32) -> Option<String> {
    let raw = fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    let s: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|c| !c.is_empty())
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    if s.is_empty() { None } else { Some(s.join(" ")) }
}
