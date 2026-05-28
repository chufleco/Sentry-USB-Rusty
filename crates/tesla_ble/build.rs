//! Compile Tesla's vendored .proto files into Rust types via prost.

fn main() {
    let protos = [
        "proto/common.proto",
        "proto/errors.proto",
        "proto/keys.proto",
        "proto/signatures.proto",
        "proto/universal_message.proto",
        "proto/vcsec.proto",
        // Needed to decode the encrypted state-query responses the car
        // returns over Infotainment.
        "proto/managed_charging.proto",
        "proto/vehicle.proto",
        "proto/car_server.proto",
    ];
    for p in &protos {
        println!("cargo:rerun-if-changed={}", p);
    }
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("locating vendored protoc binary");
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }
    // `compile_well_known_types` makes the google.protobuf.* messages
    // (Timestamp, Any, Duration, etc.) available — car_server.proto
    // imports `google/protobuf/timestamp.proto` for its
    // PreconditionSchedule message, so without this prost-build
    // fails to resolve the import.
    prost_build::Config::new()
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::prost_types")
        .compile_protos(&protos, &["proto/"])
        .expect("compiling tesla .proto files");
}
