use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

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

        // Compile custom multi-resolution .ico into the executable if rc.exe is available.
        let ico_path = out_dir.join("win_net_stress.ico");
        let ico_bytes = build_multi_res_ico(&[16, 32, 48]);
        fs::write(&ico_path, ico_bytes).expect("failed to write compile-time ico");

        let rc_path = out_dir.join("win_net_stress.rc");
        let res_path = out_dir.join("win_net_stress.res");
        let rc_content = format!(
            "1 ICON \"{}\"\n",
            ico_path.display().to_string().replace('\\', "\\\\")
        );
        fs::write(&rc_path, rc_content).expect("failed to write compile-time rc");

        if let Some(rc_exe) = find_rc_exe() {
            let ok = Command::new(rc_exe)
                .args(["/nologo", "/fo"])
                .arg(&res_path)
                .arg(&rc_path)
                .status()
                .is_ok_and(|s| s.success());
            if ok && res_path.exists() {
                println!("cargo:rustc-link-arg-bins={}", res_path.display());
            }
        }
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(path_var) = env::var("PATH") {
        for dir in env::split_paths(&path_var) {
            let cand = dir.join("rc.exe");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }

    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = fs::read_dir(kits_bin) {
        let mut versions: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path().join("x64").join("rc.exe"))
            .filter(|p| p.is_file())
            .collect();
        versions.sort();
        if let Some(latest) = versions.pop() {
            return Some(latest);
        }
    }
    None
}

fn build_multi_res_ico(sizes: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    let count = sizes.len() as u16;
    // ICONDIR
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());

    let mut images = Vec::with_capacity(sizes.len());
    for &sz in sizes {
        images.push(encode_bmp_ico_entry(sz));
    }

    let mut offset = 6 + sizes.len() * 16;
    for (&sz, img) in sizes.iter().zip(images.iter()) {
        let dim = if sz >= 256 { 0u8 } else { sz as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += img.len();
    }

    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

fn encode_bmp_ico_entry(size: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let stride_mask = size.div_ceil(32) * 4;
    let pixel_bytes = size * size * 4;
    let mask_bytes = stride_mask * size;

    // BITMAPINFOHEADER (height = size * 2 for XOR + AND masks)
    data.extend_from_slice(&40u32.to_le_bytes());
    data.extend_from_slice(&(size as i32).to_le_bytes());
    data.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&32u16.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    data.extend_from_slice(&((pixel_bytes + mask_bytes) as u32).to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());

    let pixels = render_icon_bgra(size);
    // BMP in ICO is stored bottom-up
    for y in (0..size).rev() {
        for x in 0..size {
            let [b, g, r, a] = pixels[y * size + x];
            data.push(b);
            data.push(g);
            data.push(r);
            data.push(a);
        }
    }
    data.resize(data.len() + mask_bytes, 0);
    data
}

fn render_icon_bgra(size: usize) -> Vec<[u8; 4]> {
    let mut out = vec![[0u8; 4]; size * size];
    let inv = 1.0f32 / (size as f32);

    for py in 0..size {
        for px in 0..size {
            // 2x2 supersampling for crisp anti-aliased edges at 16x16 / 32x32
            let mut acc_r = 0.0f32;
            let mut acc_g = 0.0f32;
            let mut acc_b = 0.0f32;
            let mut acc_a = 0.0f32;

            for sy in 0..2 {
                for sx in 0..2 {
                    let u = (px as f32 + (sx as f32 + 0.5) * 0.5) * inv * 2.0 - 1.0;
                    let v = (py as f32 + (sy as f32 + 0.5) * 0.5) * inv * 2.0 - 1.0;
                    let [r, g, b, a] = sample_icon_pixel(u, v);
                    acc_r += r * a;
                    acc_g += g * a;
                    acc_b += b * a;
                    acc_a += a;
                }
            }

            let a = (acc_a * 0.25).clamp(0.0, 1.0);
            let (r, g, b) = if a > 0.001 {
                (
                    (acc_r / acc_a).clamp(0.0, 1.0),
                    (acc_g / acc_a).clamp(0.0, 1.0),
                    (acc_b / acc_a).clamp(0.0, 1.0),
                )
            } else {
                (0.0, 0.0, 0.0)
            };

            out[py * size + px] = [
                (b * 255.0).round() as u8,
                (g * 255.0).round() as u8,
                (r * 255.0).round() as u8,
                (a * 255.0).round() as u8,
            ];
        }
    }
    out
}

fn sample_icon_pixel(u: f32, v: f32) -> [f32; 4] {
    // Rounded-square SDF
    let half = 0.88f32;
    let rad = 0.26f32;
    let qx = u.abs() - (half - rad);
    let qy = v.abs() - (half - rad);
    let sdf = (qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0)) - rad;
    if sdf > 0.03 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let badge_alpha = ((-sdf + 0.03) / 0.06).clamp(0.0, 1.0);

    // Rich azure-to-deep-navy diagonal gradient
    let t = ((u + v + 1.8) / 3.6).clamp(0.0, 1.0);
    let mut r = 0.02 * (1.0 - t) + 0.06 * t;
    let mut g = 0.52 * (1.0 - t) + 0.24 * t;
    let mut b = 0.82 * (1.0 - t) + 0.52 * t;

    // Subtle crisp inner border ring
    if sdf > -0.055 {
        r = r * 0.72 + 0.55 * 0.28;
        g = g * 0.72 + 0.85 * 0.28;
        b = b * 0.72 + 1.00 * 0.28;
    }

    // Outer open speed-gauge arc
    let dist = u.hypot(v);
    let angle = v.atan2(u);
    let in_arc = !(angle > 0.72 && angle < 2.42);
    if in_arc && (dist - 0.62).abs() < 0.055 {
        return [0.85, 0.95, 1.0, badge_alpha];
    }

    // Left upward arrow (Upload - crisp white)
    let up_shaft = (u + 0.19).abs() < 0.075 && v > -0.20 && v < 0.36;
    let up_head = (-0.44..=-0.10).contains(&v) && (u + 0.19).abs() <= (v + 0.44) * 0.78 + 0.02;
    if up_shaft || up_head {
        return [1.0, 1.0, 1.0, badge_alpha];
    }

    // Right downward arrow (Download - bright cyan)
    let dn_shaft = (u - 0.19).abs() < 0.075 && v > -0.36 && v < 0.20;
    let dn_head = (0.10..=0.44).contains(&v) && (u - 0.19).abs() <= (0.44 - v) * 0.78 + 0.02;
    if dn_shaft || dn_head {
        return [0.49, 0.89, 1.0, badge_alpha];
    }

    [r, g, b, badge_alpha]
}
