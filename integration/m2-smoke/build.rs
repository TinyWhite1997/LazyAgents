use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(have_codex_bin)");
    println!("cargo:rustc-check-cfg=cfg(have_opencode_bin)");

    if on_path("codex") {
        println!("cargo:rustc-cfg=have_codex_bin");
    }
    if on_path("opencode") {
        println!("cargo:rustc-cfg=have_opencode_bin");
    }
}

fn on_path(bin: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| {
        let candidate: PathBuf = dir.join(bin);
        candidate.is_file()
    })
}
