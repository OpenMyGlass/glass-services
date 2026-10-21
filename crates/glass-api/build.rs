//! Generates the Rust types from `proto/glass/services/v1/services.proto` with protox (no `protoc` needed).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let file = "glass/services/v1/services.proto";
    println!("cargo:rerun-if-changed={}", proto_root.join(file).display());
    let fds = protox::compile([file], [&proto_root])?;
    prost_build::Config::new().compile_fds(fds)?;
    Ok(())
}
