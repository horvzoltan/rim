//! Small neovim-style color schemes. Each theme is a flat palette that drives
//! both the egui chrome and the syntax highlighting.

use egui::{Color32, Stroke, Visuals};

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub dark: bool,
    pub bg: Color32,
    pub bg_dark: Color32,
    pub bg_float: Color32,
    pub bg_highlight: Color32,
    pub selection: Color32,
    pub fg: Color32,
    pub fg_dim: Color32,
    pub comment: Color32,
    pub gutter: Color32,
    pub border: Color32,
    pub red: Color32,
    pub orange: Color32,
    pub yellow: Color32,
    pub green: Color32,
    pub cyan: Color32,
    pub blue: Color32,
    pub purple: Color32,
}

const fn hex(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

macro_rules! theme {
    ($name:literal, $dark:literal, bg $bg:literal, bg_dark $bgd:literal, bg_float $bgf:literal, bg_highlight $bgh:literal,
     selection $sel:literal, fg $fg:literal, fg_dim $fgd:literal, comment $com:literal, gutter $gut:literal, border $bor:literal,
     red $r:literal, orange $o:literal, yellow $y:literal, green $g:literal, cyan $c:literal, blue $b:literal, purple $p:literal) => {
        Theme {
            name: $name,
            dark: $dark,
            bg: hex($bg),
            bg_dark: hex($bgd),
            bg_float: hex($bgf),
            bg_highlight: hex($bgh),
            selection: hex($sel),
            fg: hex($fg),
            fg_dim: hex($fgd),
            comment: hex($com),
            gutter: hex($gut),
            border: hex($bor),
            red: hex($r),
            orange: hex($o),
            yellow: hex($y),
            green: hex($g),
            cyan: hex($c),
            blue: hex($b),
            purple: hex($p),
        }
    };
}

pub fn all() -> Vec<Theme> {
    vec![
        theme!("tokyonight", true, bg 0x1a1b26, bg_dark 0x16161e, bg_float 0x1f2335, bg_highlight 0x292e42,
            selection 0x2e3c64, fg 0xc0caf5, fg_dim 0xa9b1d6, comment 0x565f89, gutter 0x3b4261, border 0x101014,
            red 0xf7768e, orange 0xff9e64, yellow 0xe0af68, green 0x9ece6a, cyan 0x7dcfff, blue 0x7aa2f7, purple 0xbb9af7),
        theme!("catppuccin-mocha", true, bg 0x1e1e2e, bg_dark 0x181825, bg_float 0x242438, bg_highlight 0x2a2b3c,
            selection 0x45475a, fg 0xcdd6f4, fg_dim 0xbac2de, comment 0x7f849c, gutter 0x45475a, border 0x11111b,
            red 0xf38ba8, orange 0xfab387, yellow 0xf9e2af, green 0xa6e3a1, cyan 0x94e2d5, blue 0x89b4fa, purple 0xcba6f7),
        theme!("gruvbox", true, bg 0x282828, bg_dark 0x1d2021, bg_float 0x32302f, bg_highlight 0x3c3836,
            selection 0x504945, fg 0xebdbb2, fg_dim 0xd5c4a1, comment 0x928374, gutter 0x665c54, border 0x141617,
            red 0xfb4934, orange 0xfe8019, yellow 0xfabd2f, green 0xb8bb26, cyan 0x8ec07c, blue 0x83a598, purple 0xd3869b),
        theme!("kanagawa", true, bg 0x1f1f28, bg_dark 0x16161d, bg_float 0x223249, bg_highlight 0x2a2a37,
            selection 0x2d4f67, fg 0xdcd7ba, fg_dim 0xc8c093, comment 0x727169, gutter 0x54546d, border 0x0f0f14,
            red 0xff5d62, orange 0xffa066, yellow 0xe6c384, green 0x98bb6c, cyan 0x7fb4ca, blue 0x7e9cd8, purple 0x957fb8),
        theme!("nord", true, bg 0x2e3440, bg_dark 0x272c36, bg_float 0x3b4252, bg_highlight 0x3b4252,
            selection 0x434c5e, fg 0xd8dee9, fg_dim 0xe5e9f0, comment 0x616e88, gutter 0x4c566a, border 0x20242c,
            red 0xbf616a, orange 0xd08770, yellow 0xebcb8b, green 0xa3be8c, cyan 0x88c0d0, blue 0x81a1c1, purple 0xb48ead),
        theme!("onedark", true, bg 0x282c34, bg_dark 0x21252b, bg_float 0x2c313a, bg_highlight 0x2c313c,
            selection 0x3e4452, fg 0xabb2bf, fg_dim 0x9da5b4, comment 0x5c6370, gutter 0x4b5263, border 0x181a1f,
            red 0xe06c75, orange 0xd19a66, yellow 0xe5c07b, green 0x98c379, cyan 0x56b6c2, blue 0x61afef, purple 0xc678dd),
        theme!("rose-pine", true, bg 0x191724, bg_dark 0x13111d, bg_float 0x1f1d2e, bg_highlight 0x21202e,
            selection 0x403d52, fg 0xe0def4, fg_dim 0x908caa, comment 0x6e6a86, gutter 0x524f67, border 0x0e0d15,
            red 0xeb6f92, orange 0xebbcba, yellow 0xf6c177, green 0xf6c177, cyan 0x9ccfd8, blue 0xebbcba, purple 0x31748f),
        theme!("everforest", true, bg 0x2d353b, bg_dark 0x232a2e, bg_float 0x343f44, bg_highlight 0x343f44,
            selection 0x475258, fg 0xd3c6aa, fg_dim 0x9da9a0, comment 0x859289, gutter 0x7a8478, border 0x1e2326,
            red 0xe67e80, orange 0xe69875, yellow 0xdbbc7f, green 0xa7c080, cyan 0x83c092, blue 0x7fbbb3, purple 0xd699b6),
        theme!("gruvbox-light", false, bg 0xfbf1c7, bg_dark 0xf2e5bc, bg_float 0xf9f5d7, bg_highlight 0xebdbb2,
            selection 0xd5c4a1, fg 0x3c3836, fg_dim 0x504945, comment 0x928374, gutter 0xbdae93, border 0xd5c4a1,
            red 0x9d0006, orange 0xaf3a03, yellow 0xb57614, green 0x79740e, cyan 0x427b58, blue 0x076678, purple 0x8f3f71),
        theme!("catppuccin-latte", false, bg 0xeff1f5, bg_dark 0xe6e9ef, bg_float 0xdce0e8, bg_highlight 0xe0e3ea,
            selection 0xbcc0cc, fg 0x4c4f69, fg_dim 0x5c5f77, comment 0x8c8fa1, gutter 0x9ca0b0, border 0xccd0da,
            red 0xd20f39, orange 0xfe640b, yellow 0xdf8e1d, green 0x40a02b, cyan 0x179299, blue 0x1e66f5, purple 0x8839ef),
    ]
}

pub fn by_name(name: &str) -> Theme {
    let all = all();
    all.iter().find(|t| t.name == name).cloned().unwrap_or_else(|| all[0].clone())
}

impl Theme {
    pub fn apply(&self, ctx: &egui::Context) {
        let mut v = if self.dark { Visuals::dark() } else { Visuals::light() };
        v.panel_fill = self.bg_dark;
        v.window_fill = self.bg_float;
        v.extreme_bg_color = self.bg;
        v.faint_bg_color = self.bg_highlight;
        v.code_bg_color = self.bg_highlight;
        v.window_stroke = Stroke::new(1.0, self.border);
        v.selection.bg_fill = self.selection;
        v.selection.stroke = Stroke::new(1.0, self.fg);
        v.hyperlink_color = self.blue;
        v.warn_fg_color = self.yellow;
        v.error_fg_color = self.red;
        v.text_cursor.stroke = Stroke::new(2.0, self.fg);

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = self.bg_dark;
        w.noninteractive.weak_bg_fill = self.bg_dark;
        w.noninteractive.bg_stroke = Stroke::new(1.0, self.border);
        w.noninteractive.fg_stroke = Stroke::new(1.0, self.fg);
        for (st, bg) in [
            (&mut w.inactive, self.bg_highlight),
            (&mut w.hovered, self.selection),
            (&mut w.active, self.selection),
            (&mut w.open, self.bg_highlight),
        ] {
            st.bg_fill = bg;
            st.weak_bg_fill = bg;
            st.fg_stroke.color = self.fg;
            st.bg_stroke.color = self.border;
        }
        w.hovered.bg_stroke = Stroke::new(1.0, self.gutter);
        // Pin egui's light/dark preference so a system theme change can't override ours.
        let which = if self.dark { egui::Theme::Dark } else { egui::Theme::Light };
        ctx.set_theme(which);
        ctx.set_visuals_of(which, v);
    }

    pub fn severity_color(&self, severity: u8) -> Color32 {
        match severity {
            1 => self.red,
            2 => self.yellow,
            3 => self.blue,
            _ => self.cyan,
        }
    }
}
