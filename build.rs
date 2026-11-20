use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/winnetstress.ico");

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
        let manifest_path = out_dir.join("win_net_stress.manifest");
        let manifest = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="WinNetStress" type="win32"/>
  <description>WinNetStress Pure In-Memory Network Tester</description>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;
        fs::write(&manifest_path, manifest).expect("failed to write compile-time manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest_path.display()
        );

        // Embed the checked-in multi-resolution icon in the PE executable.
        let source_ico = Path::new("assets").join("winnetstress.ico");
        if !source_ico.is_file() {
            panic!("required Windows application icon is missing: {}", source_ico.display());
        }
        let ico_path = out_dir.join("win_net_stress.ico");
        fs::copy(&source_ico, &ico_path).expect("failed to copy application icon");

        let rc_path = out_dir.join("win_net_stress.rc");
        let res_path = out_dir.join("win_net_stress.res");
        let rc_content = format!(
            "1 ICON \"{}\"\n",
            ico_path.display().to_string().replace('\\', "\\\\")
        );
        fs::write(&rc_path, rc_content).expect("failed to write compile-time resource script");

        let rc_exe = find_rc_exe().unwrap_or_else(|| {
            panic!("Windows SDK rc.exe was not found; cannot embed the application icon")
        });
        let status = Command::new(&rc_exe)
            .args(["/nologo", "/fo"])
            .arg(&res_path)
            .arg(&rc_path)
            .status()
            .unwrap_or_else(|e| panic!("failed to run {}: {e}", rc_exe.display()));
        if !status.success() || !res_path.is_file() {
            panic!("rc.exe failed to compile the application icon resource");
        }
        println!("cargo:rustc-link-arg-bins={}", res_path.display());
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(path_var) = env::var("PATH") {
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = fs::read_dir(kits_bin) {
        let mut versions: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("x64").join("rc.exe"))
            .filter(|path| path.is_file())
            .collect();
        versions.sort();
        versions.pop()
    } else {
        None
    }
}
