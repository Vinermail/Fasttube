fn main() {
    tauri_build::build();

    // tauri-build does not watch the frontend directory itself: without this
    // the embedded assets go stale when only ui/ changes.
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=proto/player_api.proto");
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc not available");
    // Safe: build scripts are single-threaded and this env var is set once
    // before any code reads it.
    unsafe { std::env::set_var("PROTOC", protoc) };
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .compile_protos(&["proto/player_api.proto"], &["proto"])
        .expect("failed to compile player_api.proto");
}
