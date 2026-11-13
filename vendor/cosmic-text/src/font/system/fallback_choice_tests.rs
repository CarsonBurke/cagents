use super::*;
use crate::{Buffer, Family, Metrics, Shaping, Style, Weight};
use std::{fmt::Write as _, path::PathBuf};

// Every repository font: static and variable Inter (upright and italic), Noto
// Sans for Latin, Arabic and Hebrew, and Fira Mono at 500. Never the host's.
const FILES: [&str; 7] = [
    "Inter-Regular.ttf",
    "InterVariable.ttf",
    "InterVariable-Italic.ttf",
    "NotoSans-Regular.ttf",
    "NotoSansArabic.ttf",
    "NotoSansHebrew.ttf",
    "FiraMono-Medium.ttf",
];

fn fonts() -> FontSystem {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fonts");
    let mut db = fontdb::Database::new();
    for file in FILES {
        db.load_font_data(std::fs::read(root.join(file)).unwrap());
    }
    db.set_sans_serif_family("Noto Sans");
    db.set_serif_family("Inter");
    db.set_monospace_family("Fira Mono");
    FontSystem::new_with_locale_and_db("en-US".into(), db)
}

/// The faces that draw `text`, in order, named by family, weight and style.
fn chosen(fonts: &mut FontSystem, text: &str, attrs: &Attrs<'_>) -> String {
    let mut buffer = Buffer::new_empty(Metrics::new(16.0, 20.0));
    buffer.set_size(Some(2000.0), Some(100.0));
    buffer.set_text(text, attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(fonts, true);
    let mut ids: Vec<_> = buffer
        .layout_runs()
        .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.font_id))
        .collect();
    ids.dedup();
    let names: Vec<_> = ids
        .into_iter()
        .map(|id| {
            let face = fonts.db().face(id).unwrap();
            format!("{} {} {:?}", face.families[0].0, face.weight.0, face.style)
        })
        .collect();
    names.join(", ")
}

/// Fallback's choices for every repository font, family, weight and style,
/// over Latin, Arabic, Hebrew and mixed text, recorded from upstream's
/// matching (main's vendored tree) and compared line for line, so a change
/// in the order the default, script or common fallbacks consider faces in
/// shows up here.
#[test]
fn fallback_chooses_the_faces_upstream_chose() {
    let mut fonts = fonts();
    let mut table = String::new();
    for family in [
        Family::SansSerif,
        Family::Serif,
        Family::Monospace,
        Family::Name("Inter"),
        Family::Name("Inter Variable"),
        Family::Name("Missing"),
    ] {
        for weight in [100, 300, 400, 500, 700, 900] {
            for style in [Style::Normal, Style::Italic] {
                for text in ["Hello 123", "مرحبا", "שלום", "Hi مرحبا שלום ok"] {
                    let attrs = Attrs::new().family(family).weight(Weight(weight)).style(style);
                    let faces = chosen(&mut fonts, text, &attrs);
                    writeln!(table, "{family:?} {weight} {style:?} {text}: {faces}").unwrap();
                }
            }
        }
    }
    let expected = include_str!("fallback_choices.txt");
    if table != expected {
        let differing: Vec<_> = table
            .lines()
            .zip(expected.lines())
            .filter(|(actual, expected)| actual != expected)
            .take(10)
            .collect();
        panic!(
            "fallback chose other faces ({} lines, {} expected); first differences {differing:#?}\n{table}",
            table.lines().count(),
            expected.lines().count()
        );
    }
}
