//! Pictures attached to a prompt: from the clipboard (wl-paste on Wayland, arboard elsewhere),
//! dropped or picked files; scaled to the API's limit, PNG-encoded and thumbnailed.

use std::path::Path;
use std::time::SystemTime;

use crate::claude::{ImageData, UserImage};

/// Longest side of an image sent to Claude (the API's limit is 2000 px).
pub(super) const MAX_IMAGE_SIDE: u32 = 2000;
pub(super) const THUMB_SIDE: u32 = 96;

/// A picture waiting in the prompt box.
pub struct Attachment {
    pub name: String,
    pub image: ImageData,
    pub width: usize,
    pub height: usize,
    pub texture: Option<egui::TextureHandle>,
}

impl Attachment {
    /// From raw RGBA pixels (a pasted screenshot): scaled down to the API's limit and PNG-encoded.
    pub fn from_rgba(ctx: &egui::Context, name: &str, width: u32, height: u32, rgba: &[u8]) -> Result<Attachment, String> {
        let img = image::RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or("bad image size")?;
        Self::from_image(ctx, name, img, None)
    }

    /// From a file's bytes. PNG and JPEG are decoded (for the thumbnail and size check); GIF and
    /// WebP are sent as they are.
    pub fn from_bytes(ctx: &egui::Context, name: &str, bytes: Vec<u8>) -> Result<Attachment, String> {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" => {
                let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?.to_rgba8();
                let keep = (ext == "png" && img.width().max(img.height()) <= MAX_IMAGE_SIDE).then_some(bytes);
                Self::from_image(ctx, name, img, keep)
            }
            "gif" | "webp" => Ok(Attachment {
                name: name.to_string(),
                image: ImageData { media_type: format!("image/{ext}"), bytes },
                width: 0,
                height: 0,
                texture: None,
            }),
            _ => Err(format!("{name}: not a PNG, JPEG, GIF or WebP image")),
        }
    }

    pub(super) fn from_image(ctx: &egui::Context, name: &str, mut img: image::RgbaImage, original_png: Option<Vec<u8>>) -> Result<Attachment, String> {
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return Err("empty image".into());
        }
        let longest = w.max(h);
        if longest > MAX_IMAGE_SIDE {
            let f = MAX_IMAGE_SIDE as f32 / longest as f32;
            img = image::imageops::resize(&img, ((w as f32 * f) as u32).max(1), ((h as f32 * f) as u32).max(1), image::imageops::FilterType::Triangle);
        }
        let thumb = {
            let f = THUMB_SIDE as f32 / img.width().max(img.height()) as f32;
            if f < 1.0 {
                image::imageops::resize(&img, ((img.width() as f32 * f) as u32).max(1), ((img.height() as f32 * f) as u32).max(1), image::imageops::FilterType::Triangle)
            } else {
                img.clone()
            }
        };
        let color = egui::ColorImage::from_rgba_unmultiplied([thumb.width() as usize, thumb.height() as usize], thumb.as_raw());
        let texture = ctx.load_texture(format!("chat-attachment-{name}-{}", img.width()), color, egui::TextureOptions::LINEAR);
        let bytes = match original_png {
            Some(b) => b,
            None => {
                let mut buf = std::io::Cursor::new(Vec::new());
                img.write_to(&mut buf, image::ImageFormat::Png).map_err(|e| e.to_string())?;
                buf.into_inner()
            }
        };
        Ok(Attachment { name: name.to_string(), image: ImageData { media_type: "image/png".into(), bytes }, width: img.width() as usize, height: img.height() as usize, texture: Some(texture) })
    }

    pub(super) fn user_image(&self) -> UserImage {
        UserImage { name: self.name.clone(), width: self.width, height: self.height, texture: self.texture.clone() }
    }
}

/// An image from the system clipboard, if it holds one. On Wayland the clipboard is read with
/// `wl-paste` (wl-clipboard), which sees what Wayland apps copied; elsewhere through arboard.
pub fn clipboard_image(ctx: &egui::Context) -> Option<Attachment> {
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Some(a) = wayland_clipboard_image(ctx, stamp) {
        return Some(a);
    }
    let mut cb = arboard::Clipboard::new().ok()?;
    let img = cb.get_image().ok()?;
    Attachment::from_rgba(ctx, &format!("pasted-{stamp}.png"), img.width as u32, img.height as u32, &img.bytes).ok()
}

pub(super) fn wayland_clipboard_image(ctx: &egui::Context, stamp: u64) -> Option<Attachment> {
    use std::process::{Command, Stdio};
    std::env::var_os("WAYLAND_DISPLAY")?;
    let run = |args: &[&str]| {
        let child = Command::new("wl-paste").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let out = crate::format::wait_with_timeout(child, std::time::Duration::from_secs(3)).ok()?;
        out.status.success().then_some(out.stdout)
    };
    let types = String::from_utf8_lossy(&run(&["-l"])?).to_string();
    let ty = ["image/png", "image/jpeg", "image/gif", "image/webp"].into_iter().find(|t| types.lines().any(|l| l.trim() == *t))?;
    let bytes = run(&["-t", ty]).filter(|b| !b.is_empty())?;
    let ext = match ty {
        "image/jpeg" => "jpg",
        t => t.strip_prefix("image/").unwrap_or("png"),
    };
    Attachment::from_bytes(ctx, &format!("pasted-{stamp}.{ext}"), bytes).ok()
}

pub(super) fn is_image_file(p: &Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp"))
}
