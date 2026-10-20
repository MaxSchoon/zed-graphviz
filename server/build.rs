use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_BUNDLED").is_none() {
        return;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let wasm = Path::new(&manifest_dir)
        .join("graphviz-wasm")
        .join("graphviz.wasm");
    println!("cargo::rerun-if-changed={}", wasm.display());
    if !wasm.is_file() {
        eprintln!();
        eprintln!("error: {} is missing.", wasm.display());
        eprintln!();
        eprintln!("The `bundled` feature (on by default) embeds Graphviz compiled to WebAssembly.");
        eprintln!("Build it first with:");
        eprintln!();
        eprintln!("    server/graphviz-wasm/build.sh");
        eprintln!();
        eprintln!("or build a server that only uses the system `dot` with:");
        eprintln!();
        eprintln!("    cargo build --no-default-features");
        eprintln!();
        std::process::exit(1);
    }
}
