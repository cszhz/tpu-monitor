//! 主机 CPU / 内存 / 磁盘指标(基于 sysinfo)。
use sysinfo::{Disks, System};

#[derive(Default, Clone)]
pub struct HostStats {
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub disk_mount: String,
    pub hostname: String,
}

pub struct HostMonitor {
    sys: System,
    want_mount: String,
}

impl HostMonitor {
    pub fn new(want_mount: &str) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        Self { sys, want_mount: want_mount.to_string() }
    }

    pub fn sample(&mut self) -> HostStats {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();

        let cpu_pct = self.sys.global_cpu_usage();
        let mem_used = self.sys.used_memory();
        let mem_total = self.sys.total_memory();

        // 磁盘每次重新读取列表(便宜),优先匹配目标挂载点,否则回退到 /
        let disks = Disks::new_with_refreshed_list();
        let want = std::path::Path::new(&self.want_mount);
        let root = std::path::Path::new("/");
        let chosen = disks
            .list()
            .iter()
            .find(|d| d.mount_point() == want)
            .or_else(|| disks.list().iter().find(|d| d.mount_point() == root));

        let (disk_total, disk_used, disk_mount) = match chosen {
            Some(d) => {
                let total = d.total_space();
                let avail = d.available_space();
                (
                    total,
                    total.saturating_sub(avail),
                    d.mount_point().to_string_lossy().into_owned(),
                )
            }
            None => (0, 0, "?".into()),
        };

        HostStats {
            cpu_pct,
            mem_used,
            mem_total,
            disk_used,
            disk_total,
            disk_mount,
            hostname: System::host_name().unwrap_or_else(|| "?".into()),
        }
    }
}
