use std::path::PathBuf;

fn main() {
    let proto_file = PathBuf::from("../../proto/aetherlink/v1/control.proto");
    let proto_root = PathBuf::from("../../proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());

    prost_build::Config::new()
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(&[proto_file], &[proto_root])
        .expect("failed to compile protobuf definitions");
}
