//! Small helpers: atomic file writes, git status colours, fonts.

use super::*;

/// Write via a temporary file and rename, so a crash or full disk never leaves a half-written file.
/// Symlinks are followed and the original permissions are kept.
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = target.with_file_name(format!(".{name}.rim-{}.tmp", std::process::id()));
    let perms = std::fs::metadata(&target).map(|m| m.permissions()).ok();
    let res = (|| {
        std::fs::write(&tmp, bytes)?;
        if let Some(p) = perms {
            std::fs::set_permissions(&tmp, p)?;
        }
        std::fs::rename(&tmp, &target)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

pub(super) fn status_color(t: &Theme, code: char) -> Color32 {
    match code {
        '?' | 'A' => t.green,
        'D' | 'U' => t.red,
        'R' | 'C' => t.blue,
        _ => t.yellow,
    }
}

/// A system font covering Chinese, Japanese and Korean (egui's built-in fonts don't).
pub(super) fn find_cjk_font() -> Option<std::sync::Arc<egui::FontData>> {
    let known = [
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        // macOS
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
    ];
    let path = known.iter().map(PathBuf::from).find(|p| p.is_file()).or_else(|| {
        let out = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans-serif:lang=ja"]).output().ok()?;
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        p.is_file().then_some(p)
    })?;
    let bytes = std::fs::read(path).ok()?;
    Some(std::sync::Arc::new(egui::FontData::from_owned(bytes)))
}

pub(super) static CJK_FONT: std::sync::OnceLock<Option<std::sync::Arc<egui::FontData>>> = std::sync::OnceLock::new();

pub(super) fn load_font(ctx: &egui::Context, path: Option<&str>) {
    let user = path.and_then(|p| std::fs::read(shellexpand(p)).ok()).map(|b| std::sync::Arc::new(egui::FontData::from_owned(b)));
    let build = move |cjk: Option<std::sync::Arc<egui::FontData>>| {
        let mut fonts = egui::FontDefinitions::default();
        crate::file_icons::add_to(&mut fonts);
        if let Some(u) = &user {
            fonts.font_data.insert("user-mono".into(), u.clone());
            fonts.families.entry(egui::FontFamily::Monospace).or_default().insert(0, "user-mono".into());
        }
        if let Some(c) = cjk {
            fonts.font_data.insert("cjk".into(), c);
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(fam).or_default().push("cjk".into());
            }
        }
        fonts
    };
    match CJK_FONT.get() {
        Some(c) => ctx.set_fonts(build(c.clone())),
        None => {
            ctx.set_fonts(build(None));
            if cfg!(test) {
                return;
            }
            // The CJK font is ~20 MB: read it without delaying startup.
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                if let Some(c) = CJK_FONT.get_or_init(find_cjk_font).clone() {
                    ctx.set_fonts(build(Some(c)));
                    ctx.request_repaint();
                }
            });
        }
    }
}

pub(super) fn shellexpand(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(p),
    }
}
