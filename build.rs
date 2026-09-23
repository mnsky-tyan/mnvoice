fn main() {
    // Embed the application icon so it lands in the exe's resource section.
    // Without this, the tray icon falls back to the generic OS application icon.
    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/mnvoice.ico");
    res.compile().expect("failed to compile windows resources");
}
