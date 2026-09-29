//! File-type icons: a Nerd Fonts glyph and colour per file name or extension, from
//! nvim-web-devicons. The table and the (cut-down) font are made by tools/gen_file_icons.py.

use egui::{Color32, FontFamily};

#[path = "file_icons_data.rs"]
mod data;

pub const FONT: &[u8] = include_bytes!("../assets/fonts/rim-icons.ttf");
pub const FONT_NAME: &str = "rim-icons";

pub fn family() -> FontFamily {
    FontFamily::Name(FONT_NAME.into())
}

/// Register the icon font as its own family (text never falls back to it).
pub fn add_to(fonts: &mut egui::FontDefinitions) {
    fonts.font_data.insert(FONT_NAME.into(), std::sync::Arc::new(egui::FontData::from_static(FONT)));
    fonts.families.insert(family(), vec![FONT_NAME.into()]);
}

fn lookup(table: &[(&str, char, u32, u32)], key: &str, dark: bool) -> Option<(char, Color32)> {
    let i = table.binary_search_by(|e| e.0.cmp(key)).ok()?;
    let (_, glyph, d, l) = table[i];
    let rgb = if dark { d } else { l };
    Some((glyph, Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)))
}

/// Icon and colour for a file name: an exact name match (`Cargo.toml`, `.gitignore`) first, then
/// the longest known extension (`d.ts` before `ts`), then a generic file icon.
pub fn for_file(name: &str, dark: bool) -> (char, Color32) {
    let lower = name.to_lowercase();
    lookup(data::BY_NAME, &lower, dark)
        .or_else(|| lower.match_indices('.').find_map(|(i, _)| lookup(data::BY_EXT, &lower[i + 1..], dark)))
        .unwrap_or((data::DEFAULT_FILE, Color32::from_rgb(0x6D, 0x80, 0x86)))
}

pub fn folder(open: bool) -> char {
    if open { data::FOLDER_OPEN } else { data::FOLDER }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_sorted_for_binary_search() {
        for t in [data::BY_NAME, data::BY_EXT] {
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        }
    }

    #[test]
    fn looks_up_by_name_then_longest_extension() {
        let rs = for_file("main.rs", true);
        assert_eq!(rs.0, '\u{e68b}');
        assert_eq!(for_file("MAIN.RS", true).0, rs.0);
        assert_ne!(for_file("main.rs", false).1, rs.1, "light themes get darker colours");
        assert_ne!(for_file("Dockerfile", true).0, data::DEFAULT_FILE);
        assert_ne!(for_file("README.md", true), for_file("notes.md", true), "file names beat extensions");
        assert_eq!(for_file("notes", true).0, data::DEFAULT_FILE);
        assert_eq!(for_file("notes.unknown-ext", true).0, data::DEFAULT_FILE);
    }
}
