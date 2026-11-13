//! Hover text that opens with a bold sentence (talker ADR-065).
//!
//! A hover that explains something reads best as the point, then what the
//! reader needs to weigh it. So the first paragraph is the point and is set in
//! bold, and each later paragraph is one more thing to know, set apart by a
//! blank line. Unmarked sentences of equal weight read as a wall, with nothing
//! saying which one mattered.
//!
//! Short hints — a button's action, a keyboard shortcut, a path — stay plain
//! `on_hover_text`: a bold heading over nothing is only a bold hint.
//!
//! Chrome, so both apps' hovers look alike; what each hover says stays in its
//! app.

use std::borrow::Cow;

use crate::fonts::bold;

/// What separates paragraphs. The text before the first one is the heading.
const PARAGRAPH_BREAK: &str = "\n\n";

/// Hover text whose first paragraph is a bold heading.
///
/// One string rather than a heading and a list, so a fixed hover can be a
/// `const` and a built one is a `format!`. Paragraphs are separated by a
/// blank line; a single line break stays inside a paragraph, for a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover(Cow<'static, str>);

impl Hover {
    pub const fn new(text: &'static str) -> Self {
        Self(Cow::Borrowed(text))
    }

    /// The paragraphs in order, the first being the heading.
    pub fn from_paragraphs<P: AsRef<str>>(paragraphs: impl IntoIterator<Item = P>) -> Self {
        let paragraphs: Vec<P> = paragraphs.into_iter().collect();
        let text: Vec<&str> = paragraphs.iter().map(AsRef::as_ref).collect();
        Self(Cow::Owned(text.join(PARAGRAPH_BREAK)))
    }

    /// This hover with one more paragraph at the end.
    pub fn with_paragraph(self, paragraph: &str) -> Self {
        Self(Cow::Owned(format!(
            "{}{PARAGRAPH_BREAK}{paragraph}",
            self.0
        )))
    }

    /// The whole text, as a reader sees it.
    pub fn text(&self) -> &str {
        &self.0
    }

    /// The bold first paragraph.
    pub fn heading(&self) -> &str {
        self.parts().0
    }

    fn parts(&self) -> (&str, Option<&str>) {
        match self.0.split_once(PARAGRAPH_BREAK) {
            Some((heading, rest)) => (heading, Some(rest)),
            None => (&self.0, None),
        }
    }

    /// Laid out for a tooltip: the heading in the bold face, the rest regular.
    /// The colour is left to the tooltip, as plain hover text's is.
    ///
    /// For a widget that takes its hover as text, such as a decision row or a
    /// callout; anything with a `Response` uses [`ShowHover::on_hover`].
    pub fn layout(&self, style: &egui::Style) -> egui::text::LayoutJob {
        let (heading, rest) = self.parts();
        let mut job = egui::text::LayoutJob::default();
        let mut add = |text: egui::RichText| {
            text.append_to(
                &mut job,
                style,
                egui::FontSelection::Default,
                egui::Align::Min,
            );
        };
        add(bold(heading));
        if let Some(rest) = rest {
            add(egui::RichText::new(format!("{PARAGRAPH_BREAK}{rest}")));
        }
        job
    }
}

impl From<String> for Hover {
    fn from(text: String) -> Self {
        Self(Cow::Owned(text))
    }
}

/// Attaching a [`Hover`] to a widget, as `on_hover_text` attaches plain text.
pub trait ShowHover {
    fn on_hover(self, hover: &Hover) -> Self;
}

impl ShowHover for egui::Response {
    /// What `on_hover_text` does, with the layout built only once the tooltip
    /// is actually showing.
    fn on_hover(self, hover: &Hover) -> Self {
        self.on_hover_ui(|ui| {
            // As egui's own `on_hover_text`: stops a tooltip whose text
            // changes from being shrunk by the area's auto-sizing.
            ui.set_max_width(ui.spacing().tooltip_width);
            ui.label(hover.layout(ui.style()));
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_paragraph_is_the_heading() {
        let hover = Hover::new("The point.\n\nA reason.\n\nAnother,\nas a list.");
        assert_eq!(hover.heading(), "The point.");
        assert_eq!(hover.parts().1, Some("A reason.\n\nAnother,\nas a list."));

        // A hover with only a heading has nothing after it.
        assert_eq!(Hover::new("Only this.").parts(), ("Only this.", None));
    }

    #[test]
    fn built_hovers_separate_paragraphs_with_a_blank_line() {
        let hover = Hover::from_paragraphs(["The point.", "A reason."]).with_paragraph("One more.");
        assert_eq!(hover.text(), "The point.\n\nA reason.\n\nOne more.");
        assert_eq!(Hover::from("The point.".to_owned()).heading(), "The point.");
    }

    #[test]
    fn the_layout_keeps_every_character_and_sets_only_the_heading_apart() {
        let style = egui::Style::default();
        let hover = Hover::new("The point.\n\nA reason.");
        let job = hover.layout(&style);
        assert_eq!(job.text, hover.text());
        assert_eq!(job.sections.len(), 2);
        assert_eq!(&job.text[job.sections[0].byte_range.clone()], "The point.");
        assert_ne!(
            job.sections[0].format.font_id.family, job.sections[1].format.font_id.family,
            "the heading is in the bold face"
        );
    }
}
