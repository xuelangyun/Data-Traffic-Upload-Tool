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
        let ico_bytes = build_multi_res_ico(&[16, 32, 48, 128]);
        fs::write(&ico_path, ico_bytes).expect("failed to write compile-time ico");

        let rc_path = out_dir.join("win_net_stress.rc");
        let res_path = out_dir.join("win_net_stress.res");
        let rc_content = format!(
            "1 ICON \"{}\"\n",
            ico_path.display().to_string().replace('\\', "\\\\")
        );
        fs::write(&rc_path, rc_content).expect("failed to write compile-time rc");

        let rc_exe = find_rc_exe().unwrap_or_else(|| {
            panic!("Windows SDK rc.exe was not found; cannot embed the WinNetStress icon")
        });
        let status = Command::new(&rc_exe)
            .args(["/nologo", "/fo"])
            .arg(&res_path)
            .arg(&rc_path)
            .status()
            .unwrap_or_else(|e| panic!("failed to run {}: {e}", rc_exe.display()));
        if !status.success() || !res_path.is_file() {
            panic!("rc.exe failed to compile the WinNetStress icon resource");
        }
        println!("cargo:rustc-link-arg-bins={}", res_path.display());
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
    // Deep-blue rounded square background.
    let half = 0.90f32;
    let radius = 0.25f32;
    let qx = u.abs() - (half - radius);
    let qy = v.abs() - (half - radius);
    let sdf = (qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0)) - radius;
    if sdf > 0.035 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let alpha = ((-sdf + 0.035) / 0.07).clamp(0.0, 1.0);
    let t = ((u + v + 1.9) / 3.8).clamp(0.0, 1.0);
    let color = [
        0.015 * (1.0 - t) + 0.025 * t,
        0.28 * (1.0 - t) + 0.10 * t,
        0.68 * (1.0 - t) + 0.36 * t,
        alpha,
    ];

    // Thin luminous speedometer arc across the top half.
    let gauge_y = -0.08;
    let gauge_r = 0.66;
    let gauge_dist = (u.hypot(v - gauge_y) - gauge_r).abs();
    let gauge_angle = (v - gauge_y).atan2(u);
    if gauge_angle > -2.78 && gauge_angle < -0.36 && gauge_dist < 0.045 {
        return [0.40, 0.88, 1.0, alpha];
    }

    // Gauge ticks.
    for angle in [-2.58f32, -2.08, -1.57, -1.06, -0.56] {
        let (sin_a, cos_a) = angle.sin_cos();
        let outer = gauge_r + 0.02;
        let inner = gauge_r - 0.09;
        if segment_distance(u, v, cos_a * inner, gauge_y + sin_a * inner,
                            cos_a * outer, gauge_y + sin_a * outer) < 0.022 {
            return [0.78, 0.94, 1.0, alpha];
        }
    }

    // Cyan speed needle / forward arrow.
    let shaft = segment_distance(u, v, 0.00, -0.05, 0.43, -0.49);
    let head_a = segment_distance(u, v, 0.43, -0.49, 0.26, -0.45);
    let head_b = segment_distance(u, v, 0.43, -0.49, 0.39, -0.31);
    if shaft < 0.052 || head_a < 0.052 || head_b < 0.052 {
        return [0.30, 0.94, 1.0, alpha];
    }

    // Network globe centered in the lower half.
    let globe_y = 0.23;
    let globe_r = 0.52;
    let globe_d = u.hypot(v - globe_y);
    if (globe_d - globe_r).abs() < 0.040 {
        return [0.93, 0.98, 1.0, alpha];
    }

    // Vertical meridian and curved longitude lines.
    let yn = ((v - globe_y) / globe_r).clamp(-1.0, 1.0);
    if yn.abs() < 0.96 && u.abs() < 0.030 {
        return [0.78, 0.92, 1.0, alpha];
    }
    let meridian_span = (1.0 - yn * yn).max(0.0).sqrt();
    for bend in [-0.48f32, 0.48] {
        let x = bend * globe_r * meridian_span;
        if yn.abs() < 0.90 && (u - x).abs() < 0.026 {
            return [0.78, 0.92, 1.0, alpha];
        }
    }

    // Two latitude lines.
    for offset in [-0.22f32, 0.22] {
        let width = (globe_r * globe_r - offset * offset).sqrt();
        if (v - (globe_y + offset)).abs() < 0.025 && u.abs() < width {
            return [0.78, 0.92, 1.0, alpha];
        }
    }

    // Cyan network nodes on the globe perimeter.
    for (nx, ny) in [(-0.50f32, 0.23f32), (0.50, 0.23), (0.0, 0.73)] {
        if (u - nx).hypot(v - ny) < 0.055 {
            return [0.30, 0.94, 1.0, alpha];
        }
    }

    color
}

fn segment_distance(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let length_sq = dx * dx + dy * dy;
    if length_sq <= f32::EPSILON {
        return (px - ax).hypot(py - ay);
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / length_sq).clamp(0.0, 1.0);
    (px - (ax + t * dx)).hypot(py - (ay + t * dy))
}
