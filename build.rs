fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 使用 vendored 的 protoc,免去系统安装
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);
    let wkt_include = protoc_bin_vendored::include_path()?;

    tonic_build::configure()
        .build_server(false)
        .compile_protos(
            &["proto/tpu_info/proto/tpu_metric_service.proto"],
            &["proto", wkt_include.to_str().unwrap()],
        )?;
    Ok(())
}
