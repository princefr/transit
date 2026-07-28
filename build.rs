fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/gtfs-realtime.proto");
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    prost_build::Config::new()
        .compile_protos(&["proto/gtfs-realtime.proto"], &["proto/"])?;
    Ok(())
}
