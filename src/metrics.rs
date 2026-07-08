//! 通过 gRPC 从 libtpu 运行时指标服务(默认 localhost:8431)读取指标。
//! 仅当有 TPU 负载在跑、端口开启时可用;否则返回错误,由上层降级处理。
use anyhow::Result;
use std::collections::BTreeMap;

use crate::runtime::runtime_metric_service_client::RuntimeMetricServiceClient;
use crate::runtime::{metric, MetricRequest};

pub const MEMORY_USAGE: &str = "tpu.runtime.hbm.memory.usage.bytes";
pub const TOTAL_MEMORY: &str = "tpu.runtime.hbm.memory.total.bytes";
pub const DUTY_CYCLE: &str = "tpu.runtime.tensorcore.dutycycle.percent";
pub const UPTIME: &str = "tpu.runtime.uptime.seconds.gauge";
pub const SLICE_ERROR: &str = "slice.error.detected.gauge";

/// 单个 worker(host)的一次采样。
#[derive(Default, Clone)]
pub struct Usage {
    pub usage: BTreeMap<i64, f64>, // device_id -> HBM 已用 bytes
    pub total: BTreeMap<i64, f64>, // device_id -> HBM 总量 bytes
    pub duty: BTreeMap<i64, f64>,  // device_id -> duty cycle %
    pub uptime_secs: f64,          // TPU runtime uptime(取各核最大)
    pub slice_error: f64,          // slice 错误检测(非 0 = 有错)
}

fn gauge_value(m: &crate::runtime::Metric) -> f64 {
    use crate::runtime::gauge::Value;
    if let Some(metric::Measure::Gauge(g)) = &m.measure {
        return match &g.value {
            Some(Value::AsDouble(d)) => *d,
            Some(Value::AsInt(i)) => *i as f64,
            _ => 0.0,
        };
    }
    0.0
}

fn device_id(m: &crate::runtime::Metric) -> i64 {
    use crate::runtime::attr_value::Attr;
    m.attribute
        .as_ref()
        .and_then(|a| a.value.as_ref())
        .and_then(|v| v.attr.as_ref())
        .and_then(|attr| match attr {
            Attr::IntAttr(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(-1)
}

async fn fetch_metric(
    client: &mut RuntimeMetricServiceClient<tonic::transport::Channel>,
    name: &str,
) -> Result<BTreeMap<i64, f64>> {
    let resp = client
        .get_runtime_metric(MetricRequest {
            metric_name: name.to_string(),
            skip_node_aggregation: false,
        })
        .await?;
    let mut out = BTreeMap::new();
    if let Some(tpu_metric) = resp.into_inner().metric {
        for m in &tpu_metric.metrics {
            out.insert(device_id(m), gauge_value(m));
        }
    }
    Ok(out)
}

/// 对某指标的所有条目直接取最大 gauge 值(不按 device_id 去重——
/// uptime/slice 的 attribute 是 kvlist 而非 device id,会全部塌到同一个键)。
async fn fetch_scalar_max(
    client: &mut RuntimeMetricServiceClient<tonic::transport::Channel>,
    name: &str,
) -> Result<f64> {
    let resp = client
        .get_runtime_metric(MetricRequest {
            metric_name: name.to_string(),
            skip_node_aggregation: false,
        })
        .await?;
    let mut best = 0.0_f64;
    if let Some(tpu_metric) = resp.into_inner().metric {
        for m in &tpu_metric.metrics {
            best = best.max(gauge_value(m));
        }
    }
    Ok(best)
}

pub async fn fetch(addr: &str) -> Result<Usage> {
    let mut client = RuntimeMetricServiceClient::connect(addr.to_string()).await?;
    let usage = fetch_metric(&mut client, MEMORY_USAGE).await?;
    let total = fetch_metric(&mut client, TOTAL_MEMORY).await?;
    let duty = fetch_metric(&mut client, DUTY_CYCLE).await?;
    // 这两个部分 libtpu 版本/时刻可能没数据,失败则按 0 处理。
    let uptime_secs = fetch_scalar_max(&mut client, UPTIME).await.unwrap_or(0.0);
    let slice_error = fetch_scalar_max(&mut client, SLICE_ERROR).await.unwrap_or(0.0);
    Ok(Usage {
        usage,
        total,
        duty,
        uptime_secs,
        slice_error,
    })
}
