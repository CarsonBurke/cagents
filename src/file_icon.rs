//! Original, theme-independent file glyphs. Handles are shared across every row.

use std::{path::Path, sync::LazyLock};

use cosmic::widget::icon::{self, Handle, Icon};

// Each expansion has its own lazy handle. SVG bytes and handle identity remain
// stable, allowing the renderer to cache parsing/rasterization across frames.
macro_rules! glyph {
    ($svg:literal) => {{
        static HANDLE: LazyLock<Handle> = LazyLock::new(|| {
            icon::from_svg_bytes(
                concat!(
                    r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">"#,
                    $svg,
                    "</svg>"
                )
                .as_bytes(),
            )
        });
        icon::icon(HANDLE.clone()).size(16)
    }};
}

#[derive(Clone, Copy)]
enum Kind {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    Go,
    C,
    Cpp,
    Html,
    Css,
    Json,
    Config,
    Markdown,
    Shell,
    Image,
    Docker,
    Make,
    Git,
    File,
}

fn is_one_of(value: &[u8], names: &[&[u8]]) -> bool {
    names.iter().any(|name| value.eq_ignore_ascii_case(name))
}

fn kind(path: &Path) -> Kind {
    // Compare raw encoded bytes: no lossy conversion, lowercasing allocation,
    // filesystem lookup, or assumption that a repository path is valid UTF-8.
    let name = path
        .file_name()
        .map_or(&b""[..], |name| name.as_encoded_bytes());
    if is_one_of(name, &[b"Cargo.toml", b"Cargo.lock"]) {
        return Kind::Rust;
    }
    if is_one_of(name, &[b"Dockerfile", b"Containerfile"])
        || name
            .split(|byte| *byte == b'.')
            .next()
            .is_some_and(|stem| is_one_of(stem, &[b"Dockerfile", b"Containerfile"]))
    {
        return Kind::Docker;
    }
    if is_one_of(
        name,
        &[b"Makefile", b"GNUmakefile", b"Justfile", b"CMakeLists.txt"],
    ) {
        return Kind::Make;
    }
    if is_one_of(
        name,
        &[
            b".gitignore",
            b".gitattributes",
            b".gitmodules",
            b".gitconfig",
        ],
    ) {
        return Kind::Git;
    }
    if is_one_of(
        name,
        &[b".bashrc", b".zshrc", b".bash_profile", b".profile"],
    ) {
        return Kind::Shell;
    }
    if is_one_of(name, &[b".env", b".editorconfig"])
        || name
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b".env."))
    {
        return Kind::Config;
    }
    let extension = path
        .extension()
        .map_or(&b""[..], |extension| extension.as_encoded_bytes());
    if is_one_of(extension, &[b"rs"]) {
        Kind::Rust
    } else if is_one_of(extension, &[b"ts", b"mts", b"cts"]) {
        Kind::TypeScript
    } else if is_one_of(extension, &[b"tsx"]) {
        Kind::Tsx
    } else if is_one_of(extension, &[b"js", b"mjs", b"cjs"]) {
        Kind::JavaScript
    } else if is_one_of(extension, &[b"jsx"]) {
        Kind::Jsx
    } else if is_one_of(extension, &[b"py", b"pyi", b"pyw", b"ipynb"]) {
        Kind::Python
    } else if is_one_of(extension, &[b"go"]) || is_one_of(name, &[b"go.mod", b"go.sum"]) {
        Kind::Go
    } else if is_one_of(extension, &[b"c", b"h"]) {
        Kind::C
    } else if is_one_of(extension, &[b"cpp", b"cc", b"cxx", b"hpp", b"hh", b"hxx"]) {
        Kind::Cpp
    } else if is_one_of(extension, &[b"html", b"htm", b"xhtml", b"vue", b"svelte"]) {
        Kind::Html
    } else if is_one_of(extension, &[b"css", b"scss", b"sass", b"less"]) {
        Kind::Css
    } else if is_one_of(extension, &[b"json", b"jsonc", b"json5"]) {
        Kind::Json
    } else if is_one_of(
        extension,
        &[
            b"toml", b"yaml", b"yml", b"ini", b"cfg", b"conf", b"config", b"xml", b"lock",
        ],
    ) {
        Kind::Config
    } else if is_one_of(extension, &[b"md", b"mdx", b"markdown", b"rst"]) {
        Kind::Markdown
    } else if is_one_of(
        extension,
        &[b"sh", b"bash", b"zsh", b"fish", b"ps1", b"bat", b"cmd"],
    ) {
        Kind::Shell
    } else if is_one_of(
        extension,
        &[
            b"svg", b"png", b"jpg", b"jpeg", b"gif", b"webp", b"avif", b"ico", b"bmp", b"tif",
            b"tiff",
        ],
    ) {
        Kind::Image
    } else if is_one_of(extension, &[b"mk", b"cmake"]) {
        Kind::Make
    } else {
        Kind::File
    }
}

/// A 16-pixel file glyph. Call `.size(...)` to use it in a larger heading.
/// File identity stays with the caller; the path is borrowed only for matching.
pub fn icon(path: &Path) -> Icon {
    match kind(path) {
        Kind::Rust => glyph!(
            r##"<g fill="none" stroke="#c77949" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="5.25"/><path d="M8 1v2M8 13v2M1 8h2M13 8h2M3 3l1.5 1.5M11.5 11.5 13 13M3 13l1.5-1.5M11.5 4.5 13 3M6 11V5h2.5a1.5 1.5 0 0 1 0 3H6m2.5 0 2 3"/></g>"##
        ),
        Kind::TypeScript => glyph!(
            r##"<rect x="1" y="1" width="14" height="14" rx="2" fill="#347ac0"/><path d="M3 6h5M5.5 6v6M12.5 6H10a1.5 1.5 0 0 0 0 3h1a1.5 1.5 0 0 1 0 3H8.5" fill="none" stroke="#fff" stroke-width="1.4" stroke-linejoin="round"/>"##
        ),
        Kind::Tsx => glyph!(
            r##"<g fill="none" stroke="#328fc4" stroke-width="1.2"><ellipse cx="8" cy="8" rx="7" ry="2.7"/><ellipse cx="8" cy="8" rx="7" ry="2.7" transform="rotate(60 8 8)"/><ellipse cx="8" cy="8" rx="7" ry="2.7" transform="rotate(120 8 8)"/></g><circle cx="8" cy="8" r="1.5" fill="#347ac0"/>"##
        ),
        Kind::JavaScript => glyph!(
            r##"<rect x="1" y="1" width="14" height="14" rx="2" fill="#d5ae32"/><path d="M6.5 6v4.5a1.5 1.5 0 0 1-3 0M12.5 6H10a1.5 1.5 0 0 0 0 3h1a1.5 1.5 0 0 1 0 3H8.5" fill="none" stroke="#24282e" stroke-width="1.4" stroke-linejoin="round"/>"##
        ),
        Kind::Jsx => glyph!(
            r##"<g fill="none" stroke="#b89428" stroke-width="1.2"><ellipse cx="8" cy="8" rx="7" ry="2.7"/><ellipse cx="8" cy="8" rx="7" ry="2.7" transform="rotate(60 8 8)"/><ellipse cx="8" cy="8" rx="7" ry="2.7" transform="rotate(120 8 8)"/></g><circle cx="8" cy="8" r="1.5" fill="#d5ae32"/>"##
        ),
        Kind::Python => glyph!(
            r##"<path d="M4 2.5A1.5 1.5 0 0 1 5.5 1h4A1.5 1.5 0 0 1 11 2.5v4A1.5 1.5 0 0 1 9.5 8h-5A1.5 1.5 0 0 0 3 9.5V10H2V5h6V4H4Z" fill="#4285b4"/><path d="M12 13.5a1.5 1.5 0 0 1-1.5 1.5h-4A1.5 1.5 0 0 1 5 13.5v-4A1.5 1.5 0 0 1 6.5 8h5A1.5 1.5 0 0 0 13 6.5V6h1v5H8v1h4Z" fill="#d8b642"/><circle cx="6" cy="2.75" r=".65" fill="white"/><circle cx="10" cy="13.25" r=".65" fill="#343a40"/>"##
        ),
        Kind::Go => glyph!(
            r##"<path d="M7 5H5a3 3 0 0 0 0 6h2V8H5.5M12 5a2.5 3 0 1 0 0 6 2.5 3 0 1 0 0-6M1 5h1M.5 8H1" fill="none" stroke="#2c9eaf" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
        Kind::C => glyph!(
            r##"<path d="m8 1 6 3.5v7L8 15l-6-3.5v-7Z" fill="#4e82ba"/><path d="M10 5.5a3.25 3.25 0 1 0 0 5" fill="none" stroke="#fff" stroke-width="1.8" stroke-linecap="round"/>"##
        ),
        Kind::Cpp => glyph!(
            r##"<path d="m8 1 6 3.5v7L8 15l-6-3.5v-7Z" fill="#6573b5"/><path d="M8 5.5a3 3 0 1 0 0 5M10 7v3M8.5 8.5h3M13 7v3M11.5 8.5h3" fill="none" stroke="#fff" stroke-width="1.2" stroke-linecap="round"/>"##
        ),
        Kind::Html => glyph!(
            r##"<path d="m5 4-4 4 4 4m6-8 4 4-4 4M9 2 7 14" fill="none" stroke="#d77a45" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
        Kind::Css => glyph!(
            r##"<path d="M6 2 4 14M12 2l-2 12M2 6h12M1.5 10h12" fill="none" stroke="#508ed0" stroke-width="2" stroke-linecap="round"/>"##
        ),
        Kind::Json => glyph!(
            r##"<path d="M5.5 2H4a1 1 0 0 0-1 1v3L1.5 8 3 10v3a1 1 0 0 0 1 1h1.5M10.5 2H12a1 1 0 0 1 1 1v3L14.5 8 13 10v3a1 1 0 0 1-1 1h-1.5" fill="none" stroke="#b69436" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/><circle cx="8" cy="6" r="1" fill="#b69436"/><circle cx="8" cy="10" r="1" fill="#b69436"/>"##
        ),
        Kind::Config => glyph!(
            r##"<g fill="none" stroke="#629d92" stroke-width="1.5" stroke-linecap="round"><path d="M2 4h3m4 0h5M2 8h7m4 0h1M2 12h1m4 0h7"/><circle cx="7" cy="4" r="2"/><circle cx="11" cy="8" r="2"/><circle cx="5" cy="12" r="2"/></g>"##
        ),
        Kind::Markdown => glyph!(
            r##"<rect x="1" y="3" width="14" height="10" rx="1.5" fill="none" stroke="#568cc4" stroke-width="1.4"/><path d="M3 10V6l2 2 2-2v4m4-4v4m-2-2 2 2 2-2" fill="none" stroke="#568cc4" stroke-width="1.5" stroke-linejoin="round"/>"##
        ),
        Kind::Shell => glyph!(
            r##"<rect x="1" y="2" width="14" height="12" rx="2" fill="none" stroke="#609d69" stroke-width="1.5"/><path d="m4 5 3 3-3 3m5 0h3" fill="none" stroke="#609d69" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
        Kind::Image => glyph!(
            r##"<rect x="1" y="2" width="14" height="12" rx="2" fill="none" stroke="#a078bd" stroke-width="1.5"/><circle cx="5" cy="5.5" r="1.5" fill="#a078bd"/><path d="m2 12 4-4 3 3 2-2 3 3" fill="none" stroke="#a078bd" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
        Kind::Docker => glyph!(
            r##"<path d="M2 5h3v3H2zm4 0h3v3H6zm4 0h3v3h-3zM6 1h3v3H6z" fill="#379fc4"/><path d="M1 9h11l1-2 2 1-1 2h-1c-1 3-3 4-6 4H5c-2 0-4-2-4-5Z" fill="#379fc4"/>"##
        ),
        Kind::Make => glyph!(
            r##"<path d="m3 2 4 1 1.5 3.5L14 12l-2 2-5.5-5.5L3 7 2 3l3 3 1-1Z" fill="none" stroke="#bd9754" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
        Kind::Git => glyph!(
            r##"<rect x="2.3" y="2.3" width="11.4" height="11.4" rx="1.5" transform="rotate(45 8 8)" fill="#ce7159"/><path d="M5 3.5 10 8M6 4.5V11" fill="none" stroke="#fff" stroke-width="1.3"/><g fill="#fff"><circle cx="6" cy="5" r="1.3"/><circle cx="10" cy="8" r="1.3"/><circle cx="6" cy="11" r="1.3"/></g>"##
        ),
        Kind::File => glyph!(
            r##"<path d="M9 1.5H3v13h10V5.5Zm0 0v4h4M5 9h6M5 11.5h4" fill="none" stroke="#85939f" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/>"##
        ),
    }
}

/// A matching folder glyph, with a distinct open silhouette.
pub fn folder(open: bool) -> Icon {
    if open {
        glyph!(
            r##"<path d="M1.5 12.5v-10H6L8 5h5v2" fill="none" stroke="#b18b48" stroke-width="1.4" stroke-linejoin="round"/><path d="M1.5 13.5 4 7h11l-2.5 6.5Z" fill="#c5a15a" stroke="#b18b48" stroke-width="1" stroke-linejoin="round"/>"##
        )
    } else {
        glyph!(
            r##"<path d="M1.5 3h4L7 5h7.5v8.5h-13Z" fill="#c5a15a" stroke="#b18b48" stroke-width="1.2" stroke-linejoin="round"/><path d="M2 6h12" stroke="#e1c489" stroke-width="1"/>"##
        )
    }
}
