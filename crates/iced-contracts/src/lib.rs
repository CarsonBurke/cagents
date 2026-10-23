//! Focused contracts against the UI's vendored iced core.

// Keep shared-cache artifacts tied to the checkout even outside the test harness.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

#[cfg(test)]
mod reconciliation {
    use iced::widget::{
        Id, Tree,
        tree::{NAMED, State, Tag},
    };
    use iced::{Layout, Widget, layout, mouse, renderer};
    use iced_core as iced;

    // These contracts reconcile state trees without creating or using a renderer.
    // An uninhabited renderer keeps that boundary explicit in debug and release.
    enum Renderer {}

    impl renderer::Renderer for Renderer {
        fn start_layer(&mut self, _: iced::Rectangle) {
            match *self {}
        }

        fn end_layer(&mut self) {
            match *self {}
        }

        fn start_transformation(&mut self, _: iced::Transformation) {
            match *self {}
        }

        fn end_transformation(&mut self) {
            match *self {}
        }

        fn fill_quad(&mut self, _: renderer::Quad, _: impl Into<iced::Background>) {
            match *self {}
        }

        fn reset(&mut self, _: iced::Rectangle) {
            match *self {}
        }

        fn allocate_image(
            &mut self,
            _: &iced::image::Handle,
            _: impl FnOnce(Result<iced::image::Allocation, iced::image::Error>) + Send + 'static,
        ) {
            match *self {}
        }
    }

    include!("../../../tests/support/iced_tree.rs");
}
