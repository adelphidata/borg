fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/session.proto",
        "proto/capability.proto",
        "proto/status.proto",
        "proto/admin.proto",
    ];
    // Without these, edits to .proto files don't trigger codegen on
    // incremental builds and downstream crates compile against stale stubs.
    for p in &protos {
        println!("cargo:rerun-if-changed={p}");
    }
    println!("cargo:rerun-if-changed=build.rs");

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, &["proto"])?;
    Ok(())
}
