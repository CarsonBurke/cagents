use super::*;
use crate::{Buffer, Family, Metrics, Shaping};
use std::path::{Path, PathBuf};

fn file_face(db: &fontdb::Database, path: &Path) -> fontdb::ID {
    let ids = db
        .faces()
        .filter(|face| matches!(&face.source, fontdb::Source::File(file) if file == path))
        .map(|face| face.id)
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 1, "one bundled face in {}", path.display());
    ids[0]
}

// Load only immutable repository assets, never host font discovery. Separate
// file sources make unnecessary persistent mapping promotion observable.
fn fonts(with_other_mono: bool) -> (FontSystem, fontdb::ID, Option<fontdb::ID>, fontdb::ID) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mono_path = root.join("fonts/FiraMono-Medium.ttf");
    let other_path = root.join("../libcosmic/res/noto/NotoSansMono-Regular.ttf");
    let arabic_path = root.join("fonts/NotoSansArabic.ttf");
    let mut db = fontdb::Database::new();
    for path in [&mono_path, &arabic_path] {
        db.load_font_file(path).expect("bundled font file");
    }
    let other = with_other_mono.then(|| {
        db.load_font_file(&other_path)
            .expect("bundled alternative mono");
        file_face(&db, &other_path)
    });
    let mono = file_face(&db, &mono_path);
    let arabic = file_face(&db, &arabic_path);
    assert!(db.face(mono).unwrap().monospaced);
    if let Some(other) = other {
        assert!(db.face(other).unwrap().monospaced);
    }
    let family = db.face(mono).unwrap().families[0].0.clone();
    db.set_monospace_family(family);
    (
        FontSystem::new_with_locale_and_db("en-US".into(), db),
        mono,
        other,
        arabic,
    )
}

#[derive(Debug, PartialEq)]
struct Glyph {
    start: usize,
    end: usize,
    font: fontdb::ID,
    glyph: u16,
    x: u32,
    width: u32,
}

fn shape(fonts: &mut FontSystem, mono: fontdb::ID, family: Family<'_>, text: &str) -> Vec<Glyph> {
    let weight = fonts.db().face(mono).unwrap().weight;
    // Avoid Buffer::new's unrelated empty/default-family shape before the
    // explicit test family is installed.
    let mut buffer = Buffer::new_empty(Metrics::new(16.0, 20.0));
    buffer.set_size(Some(1200.0), Some(400.0));
    buffer.set_text(
        text,
        &Attrs::new().family(family).weight(weight),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(fonts, true);
    buffer
        .layout_runs()
        .flat_map(|run| run.glyphs.iter())
        .map(|glyph| Glyph {
            start: glyph.start,
            end: glyph.end,
            font: glyph.font_id,
            glyph: glyph.glyph_id,
            x: glyph.x.to_bits(),
            width: glyph.w.to_bits(),
        })
        .collect()
}

#[test]
fn default_ascii_coverage_keeps_unselected_font_files_unpromoted() {
    let (mut fonts, mono, other, arabic) = fonts(true);
    let other = other.unwrap();
    let weight = fonts.db().face(mono).unwrap().weight;
    let text = "ASCII_123";
    assert_eq!(
        fonts.get_font_supported_codepoints_in_word(mono, weight, text),
        Some(text.chars().count()),
        "absence of an optional Unicode table is not absence of ASCII glyphs"
    );
    assert!(fonts
        .get_font(mono, weight)
        .unwrap()
        .unicode_codepoints()
        .is_empty());
    let generic = shape(&mut fonts, mono, Family::Monospace, text);
    assert!(!generic.is_empty());
    assert!(generic
        .iter()
        .all(|glyph| glyph.font == mono && glyph.glyph != 0));
    for id in [other, arabic] {
        assert!(matches!(
            &fonts.db().face(id).unwrap().source,
            fontdb::Source::File(_)
        ));
        assert!(!fonts.font_cache.keys().any(|(cached, _)| *cached == id));
    }
    let name = fonts.db().face(mono).unwrap().families[0].0.clone();
    assert_eq!(generic, shape(&mut fonts, mono, Family::Name(&name), text));
}

#[test]
fn genuinely_missing_arabic_still_uses_the_real_fallback_shaper() {
    let (mut fonts, mono, _, arabic) = fonts(false);
    let weight = fonts.db().face(mono).unwrap().weight;
    let text = "عربية";
    assert_eq!(
        fonts.get_font_supported_codepoints_in_word(mono, weight, text),
        Some(0)
    );
    assert_eq!(
        fonts.get_font_supported_codepoints_in_word(arabic, weight, text),
        Some(text.chars().count())
    );
    let glyphs = shape(&mut fonts, mono, Family::Monospace, text);
    assert!(!glyphs.is_empty());
    assert!(glyphs.iter().all(|glyph| glyph.glyph != 0));
    assert!(glyphs.iter().any(|glyph| glyph.font == arabic));
    assert!(matches!(
        &fonts.db().face(arabic).unwrap().source,
        fontdb::Source::SharedFile(_, _)
    ));
}

#[test]
fn nominal_coverage_keeps_real_complex_shaping_and_bounded_negative_cache() {
    let (mut fonts, mono, _, _) = fonts(false);
    let name = fonts.db().face(mono).unwrap().families[0].0.clone();
    for text in ["office ffi != ->", "e\u{301}", "A\u{200d}B", "👩\u{200d}💻"] {
        let generic = shape(&mut fonts, mono, Family::Monospace, text);
        assert!(!generic.is_empty());
        assert_eq!(
            generic,
            shape(&mut fonts, mono, Family::Name(&name), text),
            "{text:?}"
        );
    }
    // These bundled fonts do not claim emoji coverage. Comparing their real
    // shaping above preserves the missing-glyph behavior, not an emoji pass.
    let weight = fonts.db().face(mono).unwrap().weight;
    assert_eq!(
        fonts.get_font_supported_codepoints_in_word(mono, weight, "👩💻"),
        Some(0)
    );
    let missing = (0xF0000..0xF0500)
        .map(|cp| char::from_u32(cp).unwrap())
        .collect::<String>();
    assert_eq!(
        fonts.get_font_supported_codepoints_in_word(mono, weight, &missing),
        Some(0)
    );
    assert_eq!(
        fonts.get_font_supported_codepoints_in_word(mono, weight, &missing),
        Some(0)
    );
    let cache = &fonts.font_codepoint_support_info_cache[&mono];
    assert!(cache.supported.len() <= FontCachedCodepointSupportInfo::SUPPORTED_MAX_SZ);
    assert!(cache.not_supported.len() <= FontCachedCodepointSupportInfo::NOT_SUPPORTED_MAX_SZ);
}
