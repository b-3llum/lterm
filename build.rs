//! Embeds the logo as the Windows executable icon.

#[path = "src/logo.rs"]
mod logo;

use std::path::{Path, PathBuf};

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <consoleAllocationPolicy xmlns="http://schemas.microsoft.com/SMI/2024/WindowsSettings">detached</consoleAllocationPolicy>
    </windowsSettings>
  </application>
</assembly>
"#;

/// Write a multi-resolution .ico whose entries are PNG-compressed.
fn write_ico(path: &Path, sizes: &[u32]) {
    let pngs: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let mut buf = Vec::new();
            let mut enc = png::Encoder::new(&mut buf, s, s);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&logo::render(s)).unwrap();
            buf
        })
        .collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, png) in sizes.iter().zip(&pngs) {
        let dim = if s >= 256 { 0 } else { s as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in &pngs {
        ico.extend_from_slice(png);
    }
    std::fs::write(path, ico).unwrap();
}

fn main() {
    println!("cargo:rerun-if-changed=src/logo.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("lterm.ico");
    write_ico(&ico, &[16, 20, 24, 32, 40, 48, 64, 256]);
    // lterm is a console program (so `lterm md` can print). This asks Windows not to
    // create a console window when it's started from Explorer; older versions ignore it.
    let manifest = out.join("lterm.manifest");
    std::fs::write(&manifest, MANIFEST).unwrap();
    let path = |p: &Path| p.display().to_string().replace('\\', "/");
    let rc = out.join("lterm.rc");
    std::fs::write(&rc, format!("1 ICON \"{}\"\n1 24 \"{}\"\n", path(&ico), path(&manifest))).unwrap();
    if let Err(e) = embed_resource::compile(&rc, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=could not embed the icon: {e}");
    }
}
