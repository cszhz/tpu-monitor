//! ratatui 彩色 TUI 渲染。
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table},
    Frame,
};

use crate::host::HostStats;
use crate::sysmon::{IoStats, ProcInfo};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

pub struct DevRow {
    pub host: String,
    pub dev: i64,
    pub chip: i64,
    pub pid: String,
    pub used: f64,
    pub total: f64,
    pub duty: f64,
    pub metrics_ok: bool,
}

/// 按芯片聚合后的行:HBM 每芯片一次(2 core 共享),duty 保留每 core。
pub struct ChipRow {
    pub host: String,
    pub chip: i64,
    pub pid: String,
    pub used: f64,
    pub total: f64,
    pub metrics_ok: bool,
    pub cores: Vec<(i64, f64)>, // (core_id, duty%)
}

/// 把每 core 的 DevRow 聚合成每芯片的 ChipRow(保序)。
pub fn group_by_chip(rows: &[DevRow]) -> Vec<ChipRow> {
    let mut out: Vec<ChipRow> = Vec::new();
    for r in rows {
        match out.iter_mut().find(|c| c.chip == r.chip && c.host == r.host) {
            Some(c) => {
                if c.pid == "-" && r.pid != "-" {
                    c.pid = r.pid.clone();
                }
                if r.metrics_ok {
                    c.metrics_ok = true;
                    c.used = c.used.max(r.used);
                    c.total = c.total.max(r.total);
                }
                c.cores.push((r.dev, r.duty));
            }
            None => out.push(ChipRow {
                host: r.host.clone(),
                chip: r.chip,
                pid: r.pid.clone(),
                used: r.used,
                total: r.total,
                metrics_ok: r.metrics_ok,
                cores: vec![(r.dev, r.duty)],
            }),
        }
    }
    out
}

pub struct App {
    pub chip: String,
    pub chips: usize,
    pub cores: usize,
    pub multi_host: bool,
    pub rows: Vec<DevRow>,
    pub host: HostStats,
    pub duty_hist: Vec<u64>,
    pub hbm_hist: Vec<u64>,
    pub any_metrics: bool,
    pub procs: Vec<ProcInfo>,
    pub io: IoStats,
    pub uptime_secs: f64,
    pub slice_error: f64,
}

fn fmt_uptime(secs: f64) -> String {
    let s = secs as u64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m{}s", s % 60)
    }
}

fn fmt_time(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn fmt_mem(kb: u64) -> String {
    let mb = kb as f64 / 1024.0;
    if mb >= 1024.0 {
        format!("{:.1}G", mb / 1024.0)
    } else {
        format!("{:.0}M", mb)
    }
}

const ACCENT: Color = Color::Rgb(0x7a, 0xa2, 0xf7); // 柔和蓝
const DIM: Color = Color::Rgb(0x56, 0x5f, 0x89);

fn util_color(pct: f64) -> Color {
    if pct < 40.0 {
        Color::Rgb(0x9e, 0xce, 0x6a) // 绿
    } else if pct < 75.0 {
        Color::Rgb(0xe0, 0xaf, 0x68) // 黄
    } else {
        Color::Rgb(0xf7, 0x76, 0x8e) // 红
    }
}

/// 生成一个文本进度条,如 "███████░░░░░".
fn bar(pct: f64, width: usize) -> String {
    let p = (pct / 100.0).clamp(0.0, 1.0);
    let filled = (p * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width * 3);
    s.push_str(&"█".repeat(filled));
    s.push_str(&"░".repeat(width - filled));
    s
}

fn block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),    // device table
            Constraint::Length(9), // process panel
            Constraint::Length(6), // sparklines
            Constraint::Length(3), // host gauges + IO
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    draw_table(f, chunks[1], app);
    draw_procs(f, chunks[2], app);
    draw_history(f, chunks[3], app);
    draw_host(f, chunks[4], app);
}

fn draw_procs(f: &mut Frame, area: Rect, app: &App) {
    let headers = ["PID", "USER", "CPU%", "RES", "MEM%", "TIME+", "THR", "DEV", "COMMAND"];
    let header = Row::new(
        headers
            .into_iter()
            .map(|h| Cell::from(h).style(Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD))),
    )
    .height(1);

    let rows: Vec<Row> = app
        .procs
        .iter()
        .enumerate()
        .map(|(idx, p)| {
            let cmd: String = p.cmd.chars().take(60).collect();
            let cells = vec![
                Cell::from(p.pid.to_string()).style(Style::default().fg(ACCENT)),
                Cell::from(p.user.clone()).style(Style::default().fg(Color::Gray)),
                Cell::from(format!("{:.0}", p.cpu_pct)).style(Style::default().fg(util_color(p.cpu_pct.min(100.0)))),
                Cell::from(fmt_mem(p.rss_kb)),
                Cell::from(format!("{:.1}", p.mem_pct)),
                Cell::from(fmt_time(p.time_secs)),
                Cell::from(p.threads.to_string()).style(Style::default().fg(Color::Gray)),
                Cell::from(p.devices.clone()).style(Style::default().fg(ACCENT)),
                Cell::from(cmd).style(Style::default().fg(Color::Gray)),
            ];
            let base = if idx % 2 == 1 {
                Style::default().bg(Color::Rgb(0x1b, 0x1d, 0x2b))
            } else {
                Style::default()
            };
            Row::new(cells).height(1).style(base)
        })
        .collect();

    let title = if app.procs.is_empty() {
        " Processes (none using TPU) ".to_string()
    } else {
        format!(" Processes ({} using TPU) ", app.procs.len())
    };
    let widths = [
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .block(block(&title));
    f.render_widget(table, area);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let (dot, label, color) = if app.any_metrics {
        ("●", "ACTIVE", util_color(0.0))
    } else {
        ("○", "IDLE", DIM)
    };
    let (slice_txt, slice_color) = if app.slice_error != 0.0 {
        ("slice: ERROR!", Color::Rgb(0xf7, 0x76, 0x8e))
    } else {
        ("slice: OK", Color::Rgb(0x9e, 0xce, 0x6a))
    };
    let line = Line::from(vec![
        Span::styled("  TPU ", Style::default().fg(Color::Gray)),
        Span::styled(
            app.chip.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  chips={} cores={}", app.chips, app.cores),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("    "),
        Span::styled(format!("{dot} {label}"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(format!("   up {}", fmt_uptime(app.uptime_secs)), Style::default().fg(Color::Gray)),
        Span::raw("   "),
        Span::styled(slice_txt, Style::default().fg(slice_color).add_modifier(Modifier::BOLD)),
        Span::styled(format!("   host {}", app.host.hostname), Style::default().fg(DIM)),
    ]);
    let hint = Line::from(Span::styled(
        "[q] quit ",
        Style::default().fg(DIM),
    ))
    .alignment(Alignment::Right);
    let p = Paragraph::new(vec![line, hint]).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " ⣿ tpu-monitor ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(p, area);
}

fn draw_table(f: &mut Frame, area: Rect, app: &App) {
    let chips = group_by_chip(&app.rows);
    let multi_core = chips.iter().any(|c| c.cores.len() > 1);

    let mut headers: Vec<&str> = vec![];
    if app.multi_host {
        headers.push("HOST");
    }
    headers.push("CHIP");
    headers.extend(["PID", "HBM", "USED/TOTAL"]);
    headers.push(if multi_core { "TC Util /core" } else { "TC Util" });
    let header = Row::new(
        headers
            .into_iter()
            .map(|h| Cell::from(h).style(Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD))),
    )
    .height(1);

    let rows: Vec<Row> = chips
        .iter()
        .enumerate()
        .map(|(idx, c)| {
            let hbm_pct = if c.total > 0.0 { c.used / c.total * 100.0 } else { 0.0 };
            let mut cells: Vec<Cell> = vec![];
            if app.multi_host {
                cells.push(Cell::from(c.host.clone()).style(Style::default().fg(Color::Gray)));
            }
            cells.push(Cell::from(c.chip.to_string()).style(Style::default().fg(ACCENT)));
            cells.push(Cell::from(c.pid.clone()).style(Style::default().fg(Color::Gray)));

            if c.metrics_ok {
                cells.push(
                    Cell::from(format!("{} {:>3.0}%", bar(hbm_pct, 8), hbm_pct))
                        .style(Style::default().fg(util_color(hbm_pct))),
                );
                cells.push(
                    Cell::from(format!("{:.1}/{:.0} GiB", c.used / GIB, c.total / GIB))
                        .style(Style::default().fg(Color::Gray)),
                );
                // 每 core 的 duty
                let txt = c
                    .cores
                    .iter()
                    .map(|(id, d)| {
                        if multi_core {
                            format!("c{id}:{d:>3.0}%")
                        } else {
                            format!("{d:>3.0}%")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                let maxd = c.cores.iter().map(|(_, d)| *d).fold(0.0_f64, f64::max);
                cells.push(Cell::from(txt).style(Style::default().fg(util_color(maxd))));
            } else {
                cells.push(Cell::from(format!("{}   0%", bar(0.0, 8))).style(Style::default().fg(DIM)));
                cells.push(
                    Cell::from(format!("—/{:.0} GiB", c.total / GIB)).style(Style::default().fg(DIM)),
                );
                let txt = c
                    .cores
                    .iter()
                    .map(|(id, _)| if multi_core { format!("c{id}:  0%") } else { "  0%".into() })
                    .collect::<Vec<_>>()
                    .join("  ");
                cells.push(Cell::from(txt).style(Style::default().fg(DIM)));
            }

            let base = if idx % 2 == 1 {
                Style::default().bg(Color::Rgb(0x1b, 0x1d, 0x2b))
            } else {
                Style::default()
            };
            Row::new(cells).height(1).style(base)
        })
        .collect();

    let mut widths = vec![];
    if app.multi_host {
        widths.push(Constraint::Length(18));
    }
    widths.extend([
        Constraint::Length(5),  // CHIP
        Constraint::Length(8),  // PID
        Constraint::Length(14), // HBM bar
        Constraint::Length(14), // used/total
        Constraint::Min(16),    // TC Util per core
    ]);

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .block(block(" Chips (per-core TC Util) "));
    f.render_widget(table, area);
}

fn draw_history(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let duty_now = app.duty_hist.last().copied().unwrap_or(0);
    let duty = Sparkline::default()
        .block(block(&format!(" TC Util (duty cycle)  {duty_now}% ")))
        .data(&app.duty_hist)
        .max(100)
        .style(Style::default().fg(Color::Rgb(0x9e, 0xce, 0x6a)));
    f.render_widget(duty, cols[0]);

    let hbm_now = app.hbm_hist.last().copied().unwrap_or(0);
    let hbm = Sparkline::default()
        .block(block(&format!(" HBM usage  {hbm_now}% ")))
        .data(&app.hbm_hist)
        .max(100)
        .style(Style::default().fg(ACCENT));
    f.render_widget(hbm, cols[1]);
}

fn draw_host(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(22),
            Constraint::Percentage(26),
            Constraint::Percentage(26),
            Constraint::Percentage(26),
        ])
        .split(area);

    let h = &app.host;
    let mem_ratio = if h.mem_total > 0 {
        (h.mem_used as f64 / h.mem_total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let disk_ratio = if h.disk_total > 0 {
        (h.disk_used as f64 / h.disk_total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let cpu_ratio = (h.cpu_pct as f64 / 100.0).clamp(0.0, 1.0);

    let mk = |title: String, ratio: f64, label: String| {
        Gauge::default()
            .block(block(&title))
            .gauge_style(Style::default().fg(util_color(ratio * 100.0)).bg(Color::Rgb(0x1b, 0x1d, 0x2b)))
            .ratio(ratio)
            .label(Span::styled(label, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)))
    };

    f.render_widget(mk(" CPU ".into(), cpu_ratio, format!("{:.0}%", h.cpu_pct)), cols[0]);
    f.render_widget(
        mk(
            " RAM ".into(),
            mem_ratio,
            format!("{:.0}/{:.0} GiB", h.mem_used as f64 / GIB, h.mem_total as f64 / GIB),
        ),
        cols[1],
    );
    f.render_widget(
        mk(
            format!(" {} ", h.disk_mount),
            disk_ratio,
            format!("{:.0}/{:.0} GiB", h.disk_used as f64 / GIB, h.disk_total as f64 / GIB),
        ),
        cols[2],
    );

    // I/O 吞吐(非比例,用文本)
    let io = &app.io;
    let io_line = Line::from(vec![
        Span::styled("disk ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("R {:.0} W {:.0} ", io.disk_r, io.disk_w),
            Style::default().fg(Color::Rgb(0x9e, 0xce, 0x6a)),
        ),
        Span::styled("net ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("↓{:.0} ↑{:.0}", io.net_rx, io.net_tx),
            Style::default().fg(ACCENT),
        ),
    ]);
    let io_p = Paragraph::new(io_line).block(block(" I/O MB/s "));
    f.render_widget(io_p, cols[3]);
}
