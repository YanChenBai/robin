fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        // Oboe's prebuilt archive does not declare its C++ runtime dependency.
        println!("cargo:rustc-link-lib=dylib=c++_shared");
        // A cdylib otherwise permits unresolved symbols until the device loads it.
        println!("cargo:rustc-link-arg=-Wl,--no-undefined");
    }
}
