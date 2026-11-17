fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/logo.png");

    #[cfg(windows)]
    {
        winres::WindowsResource::new()
            .set_icon("assets/icon.ico")
            .compile()
            .expect("failed to compile Windows icon resource");
    }
}
