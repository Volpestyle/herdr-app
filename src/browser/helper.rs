//! Chromium's sandboxed renderer/GPU subprocess entry point (separate app bundle).
#[cfg(target_os = "macos")]
fn main() {
    use cef::{args::Args, *};
    if std::env::args().any(|a| a == "--print-cef-path") {
        println!(
            "{}",
            cef::sys::get_cef_dir()
                .expect("Missing compiled CEF path")
                .display()
        );
        return;
    }
    let args = Args::new();
    let mut sandbox = cef::sandbox::Sandbox::new();
    sandbox.initialize(args.as_main_args());
    let loader = library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), true);
    assert!(loader.load(), "Cannot load the bundled Chromium framework");
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    std::process::exit(execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    ));
}

#[cfg(not(target_os = "macos"))]
fn main() {}
