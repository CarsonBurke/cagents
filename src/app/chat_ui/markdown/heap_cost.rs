//! Run alone: allocator accounting is process-wide.
use super::*;
use crate::app::chat_ui::prose_document;

#[test]
#[ignore = "fw16 parser/publication live-heap census; run alone"]
fn parsed_prose_heap_cost() {
    // Initialize parser-wide lazy state outside each ownership measurement.
    drop(Content::parse(
        "A measured **plain** paragraph with `code`.\n\n",
    ));
    let atom = "A measured **plain** paragraph with `code`.\n\n";
    let mut large = atom.repeat((1024 * 1024usize).div_ceil(atom.len()));
    large.truncate(1024 * 1024);
    let inputs = [
        ("128 styled paragraphs", atom.repeat(128)),
        ("2048 styled paragraphs", atom.repeat(2048)),
        ("1 MiB styled paragraphs", large),
        ("1 MiB single paragraph", "x".repeat(1024 * 1024)),
        (
            "1 MiB single fence",
            format!("```text\n{}\n```\n", "code line\n".repeat(104858)),
        ),
    ];
    for (name, input) in inputs {
        crate::test_heap::start();
        let content = Content::parse(&input);
        let parsed = crate::test_heap::live();
        let nodes = content.items.len();
        let item_capacity = content.items.capacity();
        let (spans, span_capacity) = content
            .items()
            .iter()
            .filter_map(|item| match item {
                Item::Paragraph(text) | Item::Heading(_, text) => Some(text),
                _ => None,
            })
            .fold((0, 0), |(count, capacity), text| {
                (count + text.spans.len(), capacity + text.spans.capacity())
            });
        let publication = prose_document::Data::snapshot(&content);
        let both = crate::test_heap::live();
        drop(content);
        let published = crate::test_heap::live();
        drop(publication);
        let remaining = crate::test_heap::live();
        crate::test_heap::stop();
        eprintln!(
            "prose_heap name={name:?} input_bytes={} nodes={nodes} item_size={} text_size={} span_size={} item_capacity={item_capacity} spans={spans} span_capacity={span_capacity} parsed_bytes={parsed} parsed_plus_publication_bytes={both} publication_alone_bytes={published} remaining_bytes={remaining}",
            input.len(),
            std::mem::size_of::<Item>(),
            std::mem::size_of::<Text>(),
            std::mem::size_of::<Span>(),
        );
        assert_eq!(remaining, 0, "the census retained part of {name}");
    }
}
