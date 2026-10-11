fn main() {
    println!("cargo:rerun-if-changed=assets/robin.ico");
    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_icon("assets/robin.ico")
        .set("ProductName", "知更鸟 Robin")
        .set("FileDescription", "Robin 桌面音频串流")
        .compile()
        .expect("could not embed Robin application icon");
}
