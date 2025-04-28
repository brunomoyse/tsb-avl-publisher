// build.rs
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)           // we only need the client side
        .out_dir("src/eventpb")        // write into src/eventpb/
        .include_file("mod.rs")        // <-- generate a mod.rs!
        .compile_protos(
            &["proto/event.proto"],    // your proto(s)
            &["proto"],                // include paths
        )?;
    Ok(())
}