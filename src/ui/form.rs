//! A form a session plugin describes (`create_form`) and wrk renders: text
//! and select fields, some shown only while another field has a given value
//! (`show_if`). Submitting sends the visible fields' values to the plugin's
//! `create`. wrk knows nothing of what the fields mean.
//!
//! ```text
//! ╭ boxes: New session ───────────────────╮
//! │Box                                     │
//! │◂ dev-box ▸                        │
//! │Project                                 │
//! │◂ New project ▸                         │
//! │Name *                                  │
//! │web                               │
//! │                                        │
//! │Tab/↑↓ field  ←/→ choose  Enter create  │
//! ╰────────────────────────────────────────╯
//! ```

use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget, Wrap};
use wrk_plugin::proto::{FieldKind, FormField, FormResponse};

use super::modal::{centered_rect, field_style};
use super::{bordered, cell_width, truncate_to_width};
use crate::settings::Theme;

#[derive(Debug, Clone)]
pub struct PluginFormModal {
    /// The plugin that described the form, which `create` goes to.
    pub plugin: String,
    /// The plugin's sidebar heading, for the title.
    pub heading: String,
    /// The listed project the form was opened from (its key), if any.
    pub context: Option<String>,
    /// `None` until the plugin's description arrives.
    pub form: Option<FormResponse>,
    /// One value per field of `form`, by index: a text field's text, or the
    /// chosen option's `value`.
    pub values: Vec<String>,
    /// Index into the fields of the focused one (always a visible one).
    pub focus: usize,
    pub error: Option<String>,
    /// A `create` is in flight; the form is read-only meanwhile.
    pub submitting: bool,
}

impl PluginFormModal {
    /// A form that is still being fetched.
    pub fn loading(plugin: String, heading: String, context: Option<String>) -> Self {
        Self {
            plugin,
            heading,
            context,
            form: None,
            values: Vec::new(),
            focus: 0,
            error: None,
            submitting: false,
        }
    }

    /// The plugin's description arrived: start every field at its default.
    pub fn set_form(&mut self, form: FormResponse) {
        self.values = form
            .fields
            .iter()
            .map(|f| match f.kind {
                FieldKind::Select => f
                    .default
                    .clone()
                    .filter(|d| f.options.iter().any(|o| &o.value == d))
                    .or_else(|| f.options.first().map(|o| o.value.clone()))
                    .unwrap_or_default(),
                FieldKind::Text => f.default.clone().unwrap_or_default(),
            })
            .collect();
        self.form = Some(form);
        self.focus = 0;
        self.settle_focus();
    }

    fn fields(&self) -> &[FormField] {
        self.form.as_ref().map_or(&[], |f| f.fields.as_slice())
    }

    /// Every field's value by id (hidden ones too: they decide visibility).
    fn all_values(&self) -> BTreeMap<String, String> {
        self.fields()
            .iter()
            .zip(&self.values)
            .map(|(f, v)| (f.id.clone(), v.clone()))
            .collect()
    }

    /// Indices of the fields shown now.
    pub fn visible(&self) -> Vec<usize> {
        let values = self.all_values();
        self.fields()
            .iter()
            .enumerate()
            .filter(|(_, f)| f.visible(&values))
            .map(|(i, _)| i)
            .collect()
    }

    /// Keep focus on a visible field: the next one when it was hidden.
    fn settle_focus(&mut self) {
        let visible = self.visible();
        if !visible.contains(&self.focus) {
            self.focus = visible
                .iter()
                .copied()
                .find(|i| *i > self.focus)
                .or_else(|| visible.first().copied())
                .unwrap_or(0);
        }
    }

    fn focused_field(&self) -> Option<&FormField> {
        self.fields().get(self.focus)
    }

    /// Move focus `delta` visible fields, wrapping.
    pub fn focus_step(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let at = visible.iter().position(|i| *i == self.focus).unwrap_or(0) as isize;
        let n = visible.len() as isize;
        self.focus = visible[(at + delta).rem_euclid(n) as usize];
    }

    /// Choose the next (or previous) option of a focused select field.
    pub fn cycle(&mut self, delta: isize) {
        if self.submitting {
            return;
        }
        let Some(field) = self.focused_field().filter(|f| f.kind == FieldKind::Select) else {
            return;
        };
        if field.options.is_empty() {
            return;
        }
        let current = &self.values[self.focus];
        let at = field
            .options
            .iter()
            .position(|o| &o.value == current)
            .unwrap_or(0) as isize;
        let n = field.options.len() as isize;
        let next = field.options[(at + delta).rem_euclid(n) as usize]
            .value
            .clone();
        self.values[self.focus] = next;
        self.error = None;
        self.settle_focus();
    }

    /// The focused text field's value, for typing into.
    pub fn input_mut(&mut self) -> Option<&mut String> {
        if self.submitting || self.focused_field()?.kind != FieldKind::Text {
            return None;
        }
        self.error = None;
        self.values.get_mut(self.focus)
    }

    /// The visible fields' values for `create`, or why the form can't be sent.
    pub fn submission(&self) -> Result<BTreeMap<String, String>, String> {
        if self.form.is_none() {
            return Err("the form is still loading".into());
        }
        let mut out = BTreeMap::new();
        for i in self.visible() {
            let field = &self.fields()[i];
            let value = self.values[i].trim().to_string();
            if field.required && value.is_empty() {
                return Err(format!("{} is required", field.label));
            }
            out.insert(field.id.clone(), value);
        }
        Ok(out)
    }

    fn option_label<'a>(field: &'a FormField, value: &'a str) -> &'a str {
        field
            .options
            .iter()
            .find(|o| o.value == value)
            .map(|o| {
                if o.label.is_empty() {
                    o.value.as_str()
                } else {
                    o.label.as_str()
                }
            })
            .unwrap_or(value)
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) -> Position {
        let visible = self.visible();
        // Two rows a field, a spacer, the help and the footer.
        let wanted = (visible.len().max(1) * 2 + 4) as u16;
        let popup = centered_rect(60, 100, area);
        let height = (wanted + 2).min(area.height);
        let popup = Rect {
            y: area.y + area.height.saturating_sub(height) / 2,
            height,
            ..popup
        };
        Clear.render(popup, buf);
        let title = match &self.form {
            Some(f) if !f.title.is_empty() => format!(" {}: {} ", self.heading, f.title),
            _ => format!(" {} ", self.heading),
        };
        let block = bordered(theme)
            .title(title)
            .border_style(Style::default().fg(theme.border_focused));
        let inner = block.inner(popup);
        block.render(popup, buf);
        let width = inner.width as usize;
        let row = |i: u16| Rect {
            y: inner.y + i,
            height: 1,
            ..inner
        };
        let mut cursor = Position::new(inner.x, inner.y);

        if self.form.is_none() {
            Paragraph::new(self.error.as_deref().unwrap_or("loading the form…"))
                .style(Style::default().fg(if self.error.is_some() {
                    theme.error
                } else {
                    theme.hint
                }))
                .wrap(Wrap { trim: true })
                .render(inner, buf);
            return cursor;
        }

        let mut y = 0u16;
        for &i in &visible {
            if y + 1 >= inner.height {
                break;
            }
            let field = &self.fields()[i];
            let focused = i == self.focus && !self.submitting;
            let label = if field.required {
                format!("{} *", field.label)
            } else {
                field.label.clone()
            };
            Paragraph::new(truncate_to_width(&label, width))
                .style(if focused {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                })
                .render(row(y), buf);
            let value = &self.values[i];
            let input = row(y + 1);
            match field.kind {
                FieldKind::Select => {
                    let shown = Self::option_label(field, value);
                    let text = if focused {
                        format!("◂ {shown} ▸")
                    } else {
                        format!("  {shown}")
                    };
                    Paragraph::new(truncate_to_width(&text, width))
                        .style(field_style(focused, theme))
                        .render(input, buf);
                    if focused {
                        cursor = Position::new(input.x, input.y);
                    }
                }
                FieldKind::Text => {
                    let line = if value.is_empty() {
                        Line::from(Span::styled(
                            truncate_to_width(field.placeholder.as_deref().unwrap_or(""), width),
                            Style::default().fg(theme.hint),
                        ))
                    } else {
                        Line::from(truncate_to_width(value, width))
                    };
                    let style = if focused {
                        field_style(true, theme)
                    } else {
                        field_style(false, theme)
                    };
                    // Pad so the focused field's background spans the row.
                    Paragraph::new(line).style(style).render(input, buf);
                    if focused {
                        let x = cell_width(value).min(width.saturating_sub(1)) as u16;
                        cursor = Position::new(input.x + x, input.y);
                    }
                }
            }
            y += 2;
        }

        let help_row = row(inner.height.saturating_sub(2));
        let footer_row = row(inner.height.saturating_sub(1));
        let help = self
            .focused_field()
            .filter(|_| !self.submitting)
            .and_then(|f| f.help.clone())
            .unwrap_or_default();
        Paragraph::new(truncate_to_width(&help, width))
            .style(Style::default().fg(theme.hint))
            .render(help_row, buf);
        let submit = self
            .form
            .as_ref()
            .and_then(|f| f.submit.clone())
            .unwrap_or_else(|| "create".into())
            .to_lowercase();
        let (footer, style) = match (&self.error, self.submitting) {
            (Some(e), _) => (e.clone(), Style::default().fg(theme.error)),
            (None, true) => ("working…".to_string(), Style::default().fg(theme.hint)),
            (None, false) => (
                format!("Tab/↑↓ field  ←/→ choose  Enter {submit}  Esc cancel"),
                Style::default().fg(theme.hint),
            ),
        };
        Paragraph::new(truncate_to_width(&footer, width))
            .style(style)
            .render(footer_row, buf);
        cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrk_plugin::proto::{FormOption, ShowIf};

    fn option(value: &str, label: &str) -> FormOption {
        FormOption {
            value: value.into(),
            label: label.into(),
        }
    }

    /// The boxes plugin's shape: a box (or a new one), then a project (or a
    /// new one), with names only for the new ones.
    fn form() -> FormResponse {
        FormResponse {
            title: "New session".into(),
            fields: vec![
                FormField {
                    id: "box".into(),
                    label: "Box".into(),
                    kind: FieldKind::Select,
                    options: vec![option("i-1", "dev-box"), option("new", "New box…")],
                    default: Some("i-1".into()),
                    ..Default::default()
                },
                FormField {
                    id: "box_name".into(),
                    label: "Box name".into(),
                    required: true,
                    show_if: Some(ShowIf {
                        field: "box".into(),
                        equals: vec!["new".into()],
                    }),
                    ..Default::default()
                },
                FormField {
                    id: "name".into(),
                    label: "Name".into(),
                    required: true,
                    placeholder: Some("my-project".into()),
                    ..Default::default()
                },
            ],
            submit: None,
        }
    }

    fn modal() -> PluginFormModal {
        let mut m = PluginFormModal::loading("boxes".into(), "boxes".into(), None);
        m.set_form(form());
        m
    }

    #[test]
    fn defaults_fill_the_fields_and_hidden_ones_are_skipped() {
        let mut m = modal();
        assert_eq!(m.values, ["i-1", "", ""]);
        assert_eq!(m.visible(), [0, 2]);
        m.focus_step(1);
        assert_eq!(m.focus, 2, "skips the hidden box name");
        m.focus_step(1);
        assert_eq!(m.focus, 0, "wraps");
        m.focus_step(-1);
        assert_eq!(m.focus, 2);
    }

    #[test]
    fn choosing_an_option_reveals_its_fields() {
        let mut m = modal();
        m.cycle(1);
        assert_eq!(m.values[0], "new");
        assert_eq!(m.visible(), [0, 1, 2]);
        m.focus_step(1);
        assert_eq!(m.focus, 1);
        m.input_mut().unwrap().push_str("scratch");
        m.focus_step(-1);
        m.cycle(1); // back to dev-box: box name hides again
        assert_eq!(m.visible(), [0, 2]);
    }

    #[test]
    fn submission_sends_visible_fields_and_checks_required_ones() {
        let mut m = modal();
        assert_eq!(m.submission().unwrap_err(), "Name is required");
        m.focus_step(1);
        m.input_mut().unwrap().push_str("  web ");
        let values = m.submission().unwrap();
        assert_eq!(
            values,
            BTreeMap::from([("box".into(), "i-1".into()), ("name".into(), "web".into())])
        );
        m.focus = 0;
        m.cycle(1);
        assert_eq!(m.submission().unwrap_err(), "Box name is required");
    }

    #[test]
    fn a_submitting_form_is_read_only() {
        let mut m = modal();
        m.submitting = true;
        m.cycle(1);
        assert_eq!(m.values[0], "i-1");
        m.focus = 2;
        assert!(m.input_mut().is_none());
    }

    #[test]
    fn renders_labels_values_and_the_footer() {
        let mut m = modal();
        m.focus = 2;
        let area = Rect::new(0, 0, 100, 14);
        let mut buf = Buffer::empty(area);
        m.render(area, &mut buf, &Theme::default());
        let text: Vec<String> = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();
        let all = text.join("\n");
        assert!(all.contains("boxes: New session"), "{all}");
        assert!(all.contains("dev-box"), "{all}");
        assert!(all.contains("Name *"), "{all}");
        assert!(all.contains("my-project"), "placeholder\n{all}");
        assert!(all.contains("Enter create"), "{all}");
        assert!(!all.contains("Box name"), "hidden\n{all}");
    }
}
