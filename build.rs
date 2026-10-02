use std::path::PathBuf;

/// Find a MinGW gcc: $CC, gcc on PATH, or a winget-installed WinLibs.
fn find_mingw_bin() -> Option<PathBuf> {
    if std::env::var_os("CC").is_some() {
        return None;
    }
    if let Some(path) = std::env::var_os("PATH") {
        if std::env::split_paths(&path).any(|d| d.join("gcc.exe").is_file()) {
            return None;
        }
    }
    let pkgs = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(r"Microsoft\WinGet\Packages");
    std::fs::read_dir(pkgs)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("BrechtSanders.WinLibs"))
        .map(|e| e.path().join(r"mingw64\bin"))
        .find(|b| b.join("gcc.exe").is_file())
}

fn main() {
    let files = ["csrc/fuzzy.c", "csrc/calc.c", "csrc/raster.c", "csrc/shellhelper.c"];
    for f in files.iter().chain(["csrc/indicative.h"].iter()) {
        println!("cargo:rerun-if-changed={f}");
    }
    let mingw = find_mingw_bin();
    let mut b = cc::Build::new();
    if let Some(bin) = &mingw {
        b.compiler(bin.join("gcc.exe")).archiver(bin.join("ar.exe"));
    }
    b.files(files)
        .include("csrc")
        .opt_level(3)
        .flag("-std=gnu11")
        .flag("-fno-math-errno")
        .flag("-Wall")
        .flag("-Wno-unused-function")
        .define("WIN32_LEAN_AND_MEAN", None)
        .define("UNICODE", None)
        .define("_UNICODE", None)
        .compile("indicative_c");
    // Win32 imports used by the C code (shell32/ole32 are loaded at runtime).
    println!("cargo:rustc-link-lib=gdi32");
    println!("cargo:rustc-link-lib=user32");

    // Icon + version info.
    for f in ["assets/indicative.rc", "assets/indicative.ico"] {
        println!("cargo:rerun-if-changed={f}");
    }
    let windres = mingw.map(|b| b.join("windres.exe")).unwrap_or_else(|| PathBuf::from("windres"));
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("indicative_res.o");
    let status = std::process::Command::new(&windres)
        .args(["--input", "assets/indicative.rc", "--output-format=coff", "--include-dir", "assets", "--output"])
        .arg(&out)
        .status()
        .expect("failed to run windres");
    assert!(status.success(), "windres failed");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
