use super::*;
use crate::{Buffer, Family, Metrics, Shaping, Weight};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::path::PathBuf;

/// Font bytes that count how often anything reads them.
struct Counted {
    bytes: Vec<u8>,
    reads: Arc<AtomicUsize>,
}
impl AsRef<[u8]> for Counted {
    fn as_ref(&self) -> &[u8] {
        self.reads.fetch_add(1, Ordering::Relaxed);
        &self.bytes
    }
}

const FILES: [&str; 4] = [
    "Inter-Regular.ttf",
    "InterVariable.ttf",
    "NotoSans-Regular.ttf",
    "FiraMono-Medium.ttf",
];

// Only repository fonts, never the host's: a static Inter at 400, a variable
// Inter over 100 to 900, Noto Sans at 400 and Fira Mono at 500.
fn fonts() -> (FontSystem, Arc<AtomicUsize>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fonts");
    let reads = Arc::new(AtomicUsize::new(0));
    let mut db = fontdb::Database::new();
    for file in FILES {
        db.load_font_source(fontdb::Source::Binary(Arc::new(Counted {
            bytes: std::fs::read(root.join(file)).unwrap(),
            reads: reads.clone(),
        })));
    }
    db.set_sans_serif_family("Noto Sans");
    db.set_monospace_family("Fira Mono");
    let fonts = FontSystem::new_with_locale_and_db("en-US".into(), db);
    reads.store(0, Ordering::Relaxed);
    (fonts, reads)
}

fn face(fonts: &FontSystem, family: &str) -> fontdb::ID {
    fonts
        .db()
        .faces()
        .find(|face| face.families.iter().any(|(name, _)| name == family))
        .unwrap()
        .id
}

fn drawn_by(fonts: &mut FontSystem, family: Family<'_>, weight: Weight) -> Vec<fontdb::ID> {
    let mut buffer = Buffer::new_empty(Metrics::new(16.0, 20.0));
    buffer.set_size(Some(400.0), Some(100.0));
    buffer.set_text(
        "Hello world",
        &Attrs::new().family(family).weight(weight),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(fonts, true);
    let mut ids: Vec<_> = buffer
        .layout_runs()
        .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.font_id))
        .collect();
    ids.dedup();
    ids
}

#[test]
fn matching_a_new_weight_or_style_reads_no_font_file() {
    let (mut fonts, reads) = fonts();
    for weight in [100, 300, 400, 500, 700, 900] {
        for style in [fontdb::Style::Normal, fontdb::Style::Italic] {
            for family in [Family::SansSerif, Family::Monospace, Family::Name("Inter")] {
                let matches = fonts.get_font_matches(
                    &Attrs::new()
                        .family(family)
                        .weight(Weight(weight))
                        .style(style),
                );
                assert_eq!(matches.len(), FILES.len());
            }
        }
    }
    assert_eq!(
        reads.load(Ordering::Relaxed),
        0,
        "the face list alone orders the matches"
    );
}

#[test]
fn a_face_draws_its_own_weight_or_one_its_variable_range_covers() {
    let (mut fonts, _) = fonts();
    let variable = face(&fonts, "Inter Variable");
    let fixed = face(&fonts, "Inter");
    for weight in [100, 400, 700, 900] {
        let matches = fonts.get_font_matches(&Attrs::new().weight(Weight(weight)));
        let key = |id| *matches.iter().find(|key| key.id == id).unwrap();
        assert!(fonts.matches_weight(&key(variable), Weight(weight)));
        assert_eq!(
            fonts.matches_weight(&key(fixed), Weight(weight)),
            weight == 400
        );
    }
}

#[test]
fn a_variable_face_is_chosen_for_a_weight_it_covers() {
    let (mut fonts, _) = fonts();
    let variable = face(&fonts, "Inter Variable");
    for weight in [100, 400, 700, 900] {
        assert_eq!(
            drawn_by(&mut fonts, Family::Name("Inter Variable"), Weight(weight)),
            [variable],
            "weight {weight}"
        );
    }
    // A static face is drawn at its own weight.
    assert_eq!(
        drawn_by(&mut fonts, Family::Name("Inter"), Weight::NORMAL),
        [face(&fonts, "Inter")]
    );
}
