fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("../src-tauri/icons/icon.ico");
        // The development host cross-compiles with Homebrew MinGW; Windows
        // release hosts use the default `windres.exe` selected by winres.
        if cfg!(target_os = "macos") {
            res.set_windres_path("x86_64-w64-mingw32-windres");
            res.set_toolkit_path("/opt/homebrew/bin");
        }
        res.compile().expect("embed WhisperDrop Windows icon");
        // GNU ld drops archive members with no symbols under LTO. Link the
        // resource object directly so the icon remains in the final PE.
        // (MSVC links winres's .lib itself and has no resource.o.)
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            let out = std::env::var("OUT_DIR").expect("OUT_DIR");
            println!("cargo:rustc-link-arg={out}/resource.o");
        }
    }
}
