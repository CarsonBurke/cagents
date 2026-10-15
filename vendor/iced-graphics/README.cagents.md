Pinned from pop-os/libcosmic caec74c2559924443f12fc6faf97a5bcefe6271d, iced/graphics.
The standalone manifest preserves that workspace's versions and features.
Local addition: text::Editor::set_tab_width forwards to cosmic-text's native
buffer, invalidating selection geometry and line highlighting. This lets the
application change tab stops without rewriting source bytes or hit-test columns.
