use std::path;
fn main() -> anyhow::Result<()> {
    compile(&["protos/perfetto_trace.proto"], &["protos"]).map(drop)
}

// fn main() -> anyhow::Result<()> {
//     let proto_root = path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("protos");
//     println!("PROTO ROOT: {}", proto_root.display());
//     // https://github.com/google/perfetto/blob/main/protos/perfetto/trace/perfetto_trace.proto
//     let proto_files = &[proto_root.join("perfetto_trace.proto")];
//
//     for proto_file in proto_files {
//         println!("cargo:rerun-if-changed={}", proto_file.display());
//     }
//
//     compile(proto_files, &[proto_root])?;
//
//     Ok(())
// }

#[cfg(not(feature = "serde"))]
// Compile for `serde`: emit a file descriptor set, substitute `prost_types` for
// `pbjson_types`, and use `pbjson_build` to generate serde traits.
fn compile(
    proto_files: &[impl AsRef<path::Path>],
    includes: &[impl AsRef<path::Path>],
) -> anyhow::Result<()> {
    prost_build::Config::new()
        .bytes(["."])
        .compile_protos(proto_files, includes)?;
    Ok(())
}

#[cfg(feature = "serde")]
// Compile for `serde`: emit a file descriptor set, substitute `prost_types` for
// `pbjson_types`, and use `pbjson_build` to generate serde traits.
fn compile(
    proto_files: &[impl AsRef<path::Path>],
    includes: &[impl AsRef<path::Path>],
) -> anyhow::Result<()> {
    let descriptor_path = path::PathBuf::from(env::var("OUT_DIR")?).join("proto_descriptor.bin");

    prost_build::Config::new()
        .bytes(["."])
        .file_descriptor_set_path(&descriptor_path)
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .compile_protos(proto_files, includes)?;

    let descriptor_set = std::fs::read(descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_set)?
        .build(&[".perfetto"])?;
    Ok(())
}
