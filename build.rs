fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winres::WindowsResource::new()
            .set_icon("assets/icon/app.ico")
            // Task Manager shows this (not the exe filename) for background apps.
            .set("FileDescription", "SideQM")
            .set("ProductName", "SideQM")
            .compile()
            .expect("embed app icon");
    }
}
