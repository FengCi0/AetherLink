#![forbid(unsafe_code)]

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/aetherlink.v1.rs"));
}
