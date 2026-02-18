use std::path::PathBuf;

fn main() {
    let control_proto = PathBuf::from("../../proto/aetherlink/v1/control.proto");
    let ipc_proto = PathBuf::from("../../proto/aetherlink/v1/ipc.proto");
    let proto_root = PathBuf::from("../../proto");

    println!("cargo:rerun-if-changed={}", control_proto.display());
    println!("cargo:rerun-if-changed={}", ipc_proto.display());

    prost_build::Config::new()
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(&[control_proto, ipc_proto], &[proto_root])
        .expect("failed to compile protobuf definitions");
}
