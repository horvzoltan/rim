//! Validation: file explorer model + rendering/clicks. Included from tree.rs.

use super::*;
use egui::{Event, Pos2, RawInput, Rect};

fn setup(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rim-validate-tree-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for p in ["b_dir/inner.txt", "A_dir/x.rs", "z.txt", "a.txt", ".git/HEAD", "Ünï.txt"] {
        let p = d.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x").unwrap();
    }
    d
}

#[test]
fn rows_sorted_dirs_first_git_hidden() {
    let d = setup("rows");
    let mut t = FileTree::new(d.clone());
    let names: Vec<String> = t.flat_rows().into_iter().map(|(_, e)| e.name).collect();
    assert_eq!(names, ["A_dir", "b_dir", "a.txt", "z.txt", "Ünï.txt"]);
    // Reveal expands ancestors.
    t.reveal(&d.join("b_dir/inner.txt"));
    let rows: Vec<(usize, String)> = t.flat_rows().into_iter().map(|(depth, e)| (depth, e.name)).collect();
    assert!(rows.contains(&(1, "inner.txt".to_string())));
    // A new file shows up only after invalidate (the app invalidates every 3s and after saves).
    std::fs::write(d.join("new.txt"), "").unwrap();
    assert!(!t.flat_rows().iter().any(|(_, e)| e.name == "new.txt"));
    t.invalidate();
    assert!(t.flat_rows().iter().any(|(_, e)| e.name == "new.txt"));
    std::fs::remove_dir_all(d).ok();
}

#[test]
fn clicking_rows_opens_files_and_toggles_dirs() {
    let d = setup("click");
    let mut t = FileTree::new(d.clone());
    let theme = crate::theme::by_name("tokyonight");
    let ctx = egui::Context::default();
    let mut time = 0.0;
    let mut frame = |t: &mut FileTree, events: Vec<Event>| -> Option<TreeAction> {
        time += 0.05;
        let mut input = RawInput { events, time: Some(time), ..Default::default() };
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(300.0, 600.0)));
        let mut action = None;
        let mut out = ctx.run_ui(input, |ui| action = t.show(ui, &theme, &Default::default()));
        out.textures_delta.clear();
        action
    };
    macro_rules! click {
        ($t:expr, $row:expr) => {{
            // Rows are 22px tall starting at y = 0.
            let p = Pos2::new(50.0, 11.0 + $row as f32 * 22.0);
            frame($t, vec![Event::PointerMoved(p)]);
            frame($t, vec![Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }]);
            let a = frame($t, vec![Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }]);
            frame($t, vec![]);
            a
        }};
    }
    frame(&mut t, vec![]);
    // Row 0 = A_dir: expands.
    assert!(click!(&mut t, 0).is_none());
    assert!(t.expanded.contains(&d.join("A_dir")));
    // Row 1 is now A_dir/x.rs.
    match click!(&mut t, 1) {
        Some(TreeAction::Open(p)) => assert_eq!(p, d.join("A_dir/x.rs")),
        _ => panic!("expected open"),
    }
    // Collapse again.
    click!(&mut t, 0);
    assert!(!t.expanded.contains(&d.join("A_dir")));
    std::fs::remove_dir_all(d).ok();
}
