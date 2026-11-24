//! The device plugin API, compiled without `protoc`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/deviceplugin.proto");
    let descriptors = protox::compile(["proto/deviceplugin.proto"], ["proto"])?;
    tonic_prost_build::configure().build_client(true).build_server(true).compile_fds(descriptors)?;
    Ok(())
}
