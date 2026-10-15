//! Provider failures keep a short explanation in the transcript. The retained
//! output is built only when the reader asks to inspect it.
use super::*;

impl Runtime {
    pub(super) fn provider_exit_notice(&self, message: &str) -> bool {
        self.snapshot
            .provider_exit
            .as_ref()
            .is_some_and(|diagnostic| diagnostic.summary == message)
    }

    pub(super) fn provider_exit_view(&self) -> Option<Element<'_, Message>> {
        let diagnostic = self.snapshot.provider_exit.as_ref()?;
        let target = self.target;
        let mut content = vec![super::view::notice_entry(&diagnostic.summary, target)];
        let mut controls = Vec::new();
        if !diagnostic.output.is_empty() {
            controls.push(
                widget::button::custom(design::caption(if self.provider_exit_expanded {
                    "Hide output"
                } else {
                    "Show output"
                }))
                .class(crate::ui_controls::control_style(false, None))
                .on_press(action(Action::ToggleProviderExit(target)))
                .into(),
            );
        }
        controls.push(
            widget::button::custom(design::caption("Copy"))
                .class(crate::ui_controls::control_style(false, None))
                .on_press(action(Action::CopyProviderExit(target)))
                .into(),
        );
        content.push(
            widget::row::with_children(controls)
                .spacing(step::GAP_S)
                .wrap()
                .into(),
        );
        if self.provider_exit_expanded && !diagnostic.output.is_empty() {
            content.push(
                design::caption("Recent provider output; complete output is in the debug log.")
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
            content.push(
                widget::container(
                    widget::scrollable(
                        widget::selectable_text(diagnostic.output.as_str())
                            .size(design::TEXT_CAPTION)
                            .font(iced::Font::MONOSPACE)
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .width(Length::Fill),
                    )
                    .height(240),
                )
                .width(Length::Fill)
                .padding(step::GAP_S as u16)
                .class(design::surface(
                    Some(design::sunken),
                    [design::RADIUS_M; 4],
                    false,
                ))
                .into(),
            );
        }
        Some(
            widget::column::with_children(content)
                .spacing(step::GAP_S)
                .width(Length::Fill)
                .into(),
        )
    }
}

pub(super) fn copy_text(diagnostic: &chat::ProviderExitDiagnostic) -> String {
    if diagnostic.output.is_empty() {
        diagnostic.summary.clone()
    } else {
        format!("{}\n\n{}", diagnostic.summary, diagnostic.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_output_copy_contains_summary_and_complete_retained_output() {
        let output = format!("first line\n{}\nlast line", "diagnostic ".repeat(5_000));
        let diagnostic = chat::ProviderExitDiagnostic {
            summary: "Provider exited with status 3".into(),
            output: output.clone(),
        };
        assert_eq!(
            copy_text(&diagnostic),
            format!("{}\n\n{output}", diagnostic.summary)
        );
        assert_eq!(
            copy_text(&chat::ProviderExitDiagnostic {
                summary: "Provider was terminated".into(),
                output: String::new(),
            }),
            "Provider was terminated"
        );
    }
}
