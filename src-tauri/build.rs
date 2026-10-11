fn main() {
    // ONNX Runtime (the lyric engine's vocal isolator) links DirectML for GPU work ULTRON never asks
    // for (it runs on the CPU). Load that DLL only if it's ever used, so ULTRON always starts, even
    // where DirectML.dll isn't next to the exe (installed copies, or a dangling build link).
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:DirectML.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
    tauri_build::build()
}
