# Local iced_futures patch

Pinned source: pop-os/libcosmic caec74c2559924443f12fc6faf97a5bcefe6271d,
iced/futures. Sixteen Rust source files are byte-identical. Only event.rs and
subscription.rs add original input provenance while preserving old listen APIs.
Cargo.toml flattens that workspace's inherited dependency/package metadata.
The root Cargo patch selects this local package. The lock entry retains the
pinned dependency list; Cargo resolution still requires validation.
