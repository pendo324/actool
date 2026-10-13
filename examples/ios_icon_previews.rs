//! Write the flattened iOS Any / Dark / Tintable renders of `.icon` bundles
//! as 1024 px PNGs: `ios_icon_previews <out_dir> <bundle.icon>...` writes
//! `<out_dir>/<stem>/<appearance>.png`.

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let out = args.next().ok_or_else(|| anyhow::anyhow!("usage: ios_icon_previews <out_dir> <bundle.icon>..."))?;
    for bundle in args {
        let stem = bundle.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let dir = out.join(&stem);
        std::fs::create_dir_all(&dir)?;
        for (tag, rgba) in actool::icon_bundle::ios::render_appearances(&bundle)? {
            let img = image::RgbaImage::from_raw(1024, 1024, rgba)
                .ok_or_else(|| anyhow::anyhow!("unexpected render size"))?;
            img.save(dir.join(format!("{tag}.png")))?;
        }
    }
    Ok(())
}
