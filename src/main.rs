//! tpu-monitor —— 类 nvidia-smi 的 TPU 监控 CLI(含 ratatui 彩色 TUI)。
mod device;
mod host;
mod metrics;
mod sysmon;
mod ui;

// gRPC 生成代码:模块嵌套须与 proto package 名一致,跨包 super 链才解析得到。
pub mod tpu {
    pub mod monitoring {
        pub mod runtime {
            tonic::include_proto!("tpu.monitoring.runtime");
        }
    }
}
pub mod tpu_telemetry {
    tonic::include_proto!("tpu_telemetry");
}
pub use tpu::monitoring::runtime;

use clap::Parser;
use std::collections::VecDeque;
use std::io::Write;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use ui::{App, DevRow};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const HIST_LEN: usize = 240;

#[derive(Parser)]
#[command(name = "tpu-monitor", about = "nvidia-smi style TPU monitor")]
struct Cli {
    /// Live TUI, refresh every N seconds (omit for a one-shot static snapshot)
    #[arg(short = 'l', long)]
    watch: Option<f64>,
    /// libtpu metrics gRPC address; comma-separate multiple workers
    #[arg(long, default_value = "localhost:8431")]
    addr: String,
    /// Mount point to monitor disk usage for
    #[arg(long, default_value = "/data")]
    disk: String,
    /// (static mode) also print the command line of TPU-holding processes
    #[arg(long)]
    procs: bool,
}

fn normalize(addr: &str) -> String {
    let a = addr.trim();
    if a.starts_with("http://") || a.starts_with("https://") {
        a.to_string()
    } else {
        format!("http://{a}")
    }
}

/// 采集一轮:本地静态信息 + 各 worker 的运行时指标,组装成表行。
fn collect_rows(
    addrs: &[String],
    rt: &tokio::runtime::Runtime,
) -> (Vec<DevRow>, bool, u64, u64, f64, f64) {
    let info = device::detect();
    let owners = device::chip_owners();
    let multi = addrs.len() > 1;
    // 静态 HBM 总量(来自芯片规格),用于运行时指标不可用时的占位
    let static_total = info
        .chip
        .as_ref()
        .map(|c| c.hbm_gib as f64 * GIB)
        .unwrap_or(0.0);

    let mut rows = Vec::new();
    let mut any_metrics = false;
    let mut duty_sum = 0.0;
    let mut hbm_sum = 0.0;
    let mut n_metrics = 0usize;
    let mut uptime_secs = 0.0_f64;
    let mut slice_error = 0.0_f64;

    for (hi, addr) in addrs.iter().enumerate() {
        let is_local = hi == 0;
        let host_label = addr
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string();
        let usage = rt.block_on(metrics::fetch(addr)).ok();
        if let Some(u) = &usage {
            any_metrics = true;
            uptime_secs = uptime_secs.max(u.uptime_secs);
            slice_error = slice_error.max(u.slice_error);
        }

        // 设备数量:本地用 PCI 检测,远程用指标里的 device 数
        let count = if is_local {
            info.cores.max(usage.as_ref().map(|u| u.usage.len()).unwrap_or(0))
        } else {
            usage.as_ref().map(|u| u.usage.len()).unwrap_or(0)
        };

        for i in 0..count {
            let dev = i as i64;
            let pid = if is_local {
                owners
                    .get(&format!("/dev/vfio/{i}"))
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into())
            } else {
                "-".into()
            };
            let (used, total, duty, ok) = match &usage {
                Some(u) => {
                    let used = u.usage.get(&dev).copied().unwrap_or(0.0);
                    let total = u.total.get(&dev).copied().unwrap_or(0.0);
                    let duty = u.duty.get(&dev).copied().unwrap_or(0.0);
                    (used, total, duty, true)
                }
                // 无运行时指标:HBM 总量用静态规格,其余按空闲(0)展示
                None => (0.0, static_total, 0.0, false),
            };
            if ok {
                duty_sum += duty;
                hbm_sum += if total > 0.0 { used / total * 100.0 } else { 0.0 };
                n_metrics += 1;
            }
            rows.push(DevRow {
                host: host_label.clone(),
                dev,
                pid,
                used,
                total,
                duty,
                metrics_ok: ok,
            });
        }
    }

    let avg_duty = if n_metrics > 0 { (duty_sum / n_metrics as f64) as u64 } else { 0 };
    let avg_hbm = if n_metrics > 0 { (hbm_sum / n_metrics as f64) as u64 } else { 0 };
    let _ = multi;
    (rows, any_metrics, avg_duty, avg_hbm, uptime_secs, slice_error)
}

fn chip_label() -> (String, usize, usize) {
    let info = device::detect();
    (
        info.chip.as_ref().map(|c| c.name.to_string()).unwrap_or_else(|| "unknown".into()),
        info.chips,
        info.cores,
    )
}

fn run_tui(cli: &Cli, addrs: Vec<String>, rt: tokio::runtime::Runtime) -> anyhow::Result<()> {
    let interval = Duration::from_secs_f64(cli.watch.unwrap());
    let mut hostmon = host::HostMonitor::new(&cli.disk);
    let mut sysmon = sysmon::SysMon::new(&cli.disk);
    let (chip, chips, cores) = chip_label();
    let multi_host = addrs.len() > 1;
    let mut duty_hist: VecDeque<u64> = VecDeque::with_capacity(HIST_LEN);
    let mut hbm_hist: VecDeque<u64> = VecDeque::with_capacity(HIST_LEN);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let res = (|| -> anyhow::Result<()> {
        loop {
            let (rows, any_metrics, avg_duty, avg_hbm, uptime_secs, slice_error) =
                collect_rows(&addrs, &rt);
            if duty_hist.len() == HIST_LEN {
                duty_hist.pop_front();
                hbm_hist.pop_front();
            }
            duty_hist.push_back(avg_duty);
            hbm_hist.push_back(avg_hbm);
            let host_stats = hostmon.sample();
            let owners = device::chip_owners();
            let (procs, io) = sysmon.sample(&owners);

            let app = App {
                chip: chip.clone(),
                chips,
                cores,
                multi_host,
                rows,
                host: host_stats,
                duty_hist: duty_hist.iter().copied().collect(),
                hbm_hist: hbm_hist.iter().copied().collect(),
                any_metrics,
                procs,
                io,
                uptime_secs,
                slice_error,
            };
            terminal.draw(|f| ui::draw(f, &app))?;

            if event::poll(interval)? {
                if let Event::Key(k) = event::read()? {
                    let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    res
}

fn print_once(cli: &Cli, addrs: &[String], rt: &tokio::runtime::Runtime) {
    use comfy_table::{presets::UTF8_FULL, Table};
    let (chip, chips, cores) = chip_label();
    let multi = addrs.len() > 1;
    let (rows, any_metrics, _, _, uptime_secs, slice_error) = collect_rows(addrs, rt);

    let up = uptime_secs as u64;
    let uptime = if up >= 3600 {
        format!("{}h{}m", up / 3600, (up % 3600) / 60)
    } else {
        format!("{}m", up / 60)
    };
    let slice = if slice_error != 0.0 { "ERROR!" } else { "OK" };
    println!(
        "TPU {chip}   chips={chips} cores={cores}   up {uptime}   slice: {slice}   host={}",
        host::HostMonitor::new(&cli.disk).sample().hostname
    );

    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    let mut header: Vec<&str> = vec![];
    if multi {
        header.push("Host");
    }
    header.extend(["Core", "PID", "HBM used/total (GiB)", "TC Util%"]);
    table.set_header(header);

    for r in &rows {
        let mut cells: Vec<String> = vec![];
        if multi {
            cells.push(r.host.clone());
        }
        cells.push(r.dev.to_string());
        cells.push(r.pid.clone());
        if r.metrics_ok {
            cells.push(format!("{:.1}/{:.0} GiB", r.used / GIB, r.total / GIB));
            cells.push(format!("{:.1}", r.duty));
        } else {
            // 空闲:HBM 总量用静态值,其余按 0
            cells.push(format!("—/{:.0} GiB", r.total / GIB));
            cells.push("0".into());
        }
        table.add_row(cells);
    }
    println!("{table}");
    if !any_metrics {
        println!("\x1b[33m(runtime metrics unavailable: no TPU workload / port 8431 down)\x1b[0m");
    }

    // CPU% 与 I/O 需要两次采样算增量:先 prime,再取第二次
    let mut hostmon = host::HostMonitor::new(&cli.disk);
    let mut sysmon = sysmon::SysMon::new(&cli.disk);
    let owners = device::chip_owners();
    let _ = hostmon.sample();
    let _ = sysmon.sample(&owners);
    std::thread::sleep(Duration::from_millis(400));
    let h = hostmon.sample();
    let (procs, io) = sysmon.sample(&owners);

    println!(
        "Host: CPU {:.0}%   RAM {:.0}/{:.0} GiB   {} {:.0}/{:.0} GiB   | disk R {:.0} W {:.0} MB/s  net ↓{:.0} ↑{:.0} MB/s",
        h.cpu_pct,
        h.mem_used as f64 / GIB,
        h.mem_total as f64 / GIB,
        h.disk_mount,
        h.disk_used as f64 / GIB,
        h.disk_total as f64 / GIB,
        io.disk_r,
        io.disk_w,
        io.net_rx,
        io.net_tx,
    );

    if procs.is_empty() {
        println!("Processes: (none using TPU)");
    } else {
        println!(
            "{:<8}{:<10}{:>6}{:>8}{:>6}{:>9}{:>5}  {:<6} COMMAND",
            "PID", "USER", "CPU%", "RES", "MEM%", "TIME+", "THR", "DEV"
        );
        for p in &procs {
            let cmd: String = p.cmd.chars().take(70).collect();
            let res = if p.rss_kb >= 1024 * 1024 {
                format!("{:.1}G", p.rss_kb as f64 / 1024.0 / 1024.0)
            } else {
                format!("{:.0}M", p.rss_kb as f64 / 1024.0)
            };
            let secs = p.time_secs;
            let time = format!("{}:{:02}", secs / 60, secs % 60);
            println!(
                "{:<8}{:<10}{:>6.0}{:>8}{:>6.1}{:>9}{:>5}  {:<6} {}",
                p.pid, p.user, p.cpu_pct, res, p.mem_pct, time, p.threads, p.devices, cmd
            );
        }
    }
    let _ = cli.procs;
    std::io::stdout().flush().ok();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let addrs: Vec<String> = cli.addr.split(',').map(normalize).collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    if cli.watch.is_some() {
        run_tui(&cli, addrs, rt)?;
    } else {
        print_once(&cli, &addrs, &rt);
    }
    Ok(())
}
