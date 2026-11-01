fn main() {
    slint_build::compile("ui/app.slint").expect("Slint UI compilation failed");
    #[cfg(windows)]
    embed_icon();
}

/// Links `packaging/app.ico` into the program with the Windows SDK's rc.exe.
/// Without the SDK the app still builds, just with the default Explorer icon.
#[cfg(windows)]
fn embed_icon() {
    use std::{env, fs, path::PathBuf, process::Command};
    println!("cargo:rerun-if-changed=packaging/app.ico");
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let compiler = fs::read_dir(kits)
        .into_iter()
        .flatten()
        .flatten()
        .map(|version| version.path().join(r"x64\rc.exe"))
        .filter(|path| path.is_file())
        .max();
    let Some(compiler) = compiler else {
        println!("cargo:warning=rc.exe not found; the program will have no embedded icon");
        return;
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join(r"packaging\app.ico")
        .display()
        .to_string()
        .replace('\\', r"\\");
    let script = out.join("app.rc");
    fs::write(&script, format!("1 ICON \"{icon}\"\n")).unwrap();
    let resource = out.join("app.res");
    let status = Command::new(compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg(&script)
        .status()
        .expect("rc.exe could not start");
    assert!(status.success(), "rc.exe failed");
    println!("cargo:rustc-link-arg-bins={}", resource.display());
}
