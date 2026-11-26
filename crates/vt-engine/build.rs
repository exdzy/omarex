use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Locates `libghostty-vt` headers and shared library cleanly on Arch/Omarchy.
///
/// Arch's `ghostty 1.4.0-1` package currently ships `/usr/share/pkgconfig/libghostty-vt.pc`
/// with a broken build-time staging prefix (`/src/ghostty/src/ghostty-1.4.0/build/usr`),
/// so we probe explicit environment overrides and standard system paths directly before
/// falling back to `pkg-config` (and only accept `pkg-config` if its paths actually exist).
fn main() {
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_LIB_DIR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_VT_STATIC");

    let include_dir = resolve_include_dir();
    let lib_dir = resolve_lib_dir();

    println!(
        "cargo:rerun-if-changed={}",
        include_dir.join("ghostty/vt.h").display()
    );
    println!(
        "cargo:rustc-env=GHOSTTY_VT_INCLUDE_DIR={}",
        include_dir.display()
    );
    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    let use_static = env::var("GHOSTTY_VT_STATIC")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if use_static {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    } else {
        println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    }
}

fn resolve_include_dir() -> PathBuf {
    if let Ok(dir) = env::var("GHOSTTY_VT_INCLUDE_DIR") {
        let p = PathBuf::from(&dir);
        assert!(
            p.join("ghostty/vt.h").is_file(),
            "GHOSTTY_VT_INCLUDE_DIR={} does not contain ghostty/vt.h",
            p.display()
        );
        return p;
    }

    for candidate in ["/usr/include", "/usr/local/include"] {
        let p = Path::new(candidate);
        if p.join("ghostty/vt.h").is_file() {
            return p.to_path_buf();
        }
    }

    if let Some(p) = pkg_config_variable("includedir")
        && p.join("ghostty/vt.h").is_file()
    {
        return p;
    }

    panic!(
        "Could not locate ghostty/vt.h. Install the `ghostty` package (libghostty-vt) \
         or set GHOSTTY_VT_INCLUDE_DIR."
    );
}

fn resolve_lib_dir() -> PathBuf {
    if let Ok(dir) = env::var("GHOSTTY_VT_LIB_DIR") {
        let p = PathBuf::from(&dir);
        assert!(
            has_ghostty_lib(&p),
            "GHOSTTY_VT_LIB_DIR={} does not contain libghostty-vt.so or libghostty-vt.a",
            p.display()
        );
        return p;
    }

    for candidate in ["/usr/lib", "/usr/lib64", "/usr/local/lib"] {
        let p = Path::new(candidate);
        if has_ghostty_lib(p) {
            return p.to_path_buf();
        }
    }

    if let Some(p) = pkg_config_variable("libdir")
        && has_ghostty_lib(&p)
    {
        return p;
    }

    panic!(
        "Could not locate libghostty-vt.so. Install the `ghostty` package \
         or set GHOSTTY_VT_LIB_DIR."
    );
}

fn has_ghostty_lib(dir: &Path) -> bool {
    dir.join("libghostty-vt.so").exists() || dir.join("libghostty-vt.a").exists()
}

fn pkg_config_variable(var: &str) -> Option<PathBuf> {
    let output = Command::new("pkg-config")
        .args(["--variable", var, "libghostty-vt"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    path.exists().then_some(path)
}
