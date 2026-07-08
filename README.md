# tpu-monitor

nvidia-smi 风格的 TPU 监控 CLI(Rust)。支持 v5p/v6e/v7x(区分 chip/core)。

## 直接用预编译二进制(无需 cargo)
    ./dist/tpu-monitor          # 静态快照
    ./dist/tpu-monitor -l 1     # 实时 TUI

## 从源码构建
    cargo build --release       # 产物在 target/release/tpu-monitor
