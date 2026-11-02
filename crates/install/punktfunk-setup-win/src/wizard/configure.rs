//! Configure: the settings sections and their editors.

use super::*;

/// One titled section: its rows in a single card, divided, never a card per row.
fn section(
    w: &Wizard,
    cx: &ViewContext<'_, Wizard>,
    title: &str,
    fields: &[Field],
    lead: Option<View>,
) -> View {
    let mut rows: Vec<View> = Vec::new();
    if let Some(lead) = lead {
        rows.push(lead);
    }
    for field in fields {
        let row = config_row(w, cx, *field);
        rows.push(if rows.is_empty() {
            row
        } else {
            Border::new()
                .border_brush(w.palette.get(Token::DividerStroke))
                .border_thickness(Thickness::new(0.0, 1.0, 0.0, 0.0))
                .padding(Thickness::new(0.0, 10.0, 0.0, 0.0))
                .margin(Thickness::new(0.0, 10.0, 0.0, 0.0))
                .content(row)
                .into()
        });
    }
    StackPanel::new()
        .children((
            note(w, title)
                .font_size(12.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .margin(Thickness::new(2.0, 0.0, 0.0, 6.0)),
            card(StackPanel::new().children(rows)),
        ))
        .into()
}

pub(super) fn configure_page(w: &Wizard, cx: &ViewContext<'_, Wizard>) -> View {
    // The D11 coexistence note leads the Network section: a visible row, never a dialog.
    let coexistence = w.screen.coexistence_note().map(|text| {
        StackPanel::new()
            .spacing(2.0)
            .children((
                TextBlock::new()
                    .text("Runs next to your other streaming host")
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(w.palette.get(Token::SystemAttention)),
                note(w, text).font_size(12.0),
            ))
            .into()
    });
    let mut columns: [Vec<View>; 2] = [Vec::new(), Vec::new()];
    for (i, (title, fields)) in w.screen.sections().into_iter().enumerate() {
        let lead = if title == "Network" {
            coexistence.clone()
        } else {
            None
        };
        columns[i % 2].push(section(w, cx, title, &fields, lead));
    }
    let [left, right] = columns;
    let body: View = if right.is_empty() {
        StackPanel::new()
            .spacing(18.0)
            .max_width(560.0)
            .children(left)
            .into()
    } else {
        Grid::new()
            .columns([GridLength::STAR, GridLength::STAR])
            .column_spacing(18.0)
            .children((
                StackPanel::new()
                    .spacing(18.0)
                    .grid_column(0)
                    .children(left),
                StackPanel::new()
                    .spacing(18.0)
                    .grid_column(1)
                    .children(right),
            ))
            .into()
    };
    frame(
        w,
        WizStep::Configure,
        ScrollView::new().content(body),
        vec![
            back_button(cx, WizStep::Configure),
            continue_button(cx, WizStep::Configure, w.go_label(WizStep::Configure)),
        ],
    )
}

fn config_row(w: &Wizard, cx: &ViewContext<'_, Wizard>, field: Field) -> View {
    let editor: View = match w.screen.editor(field) {
        Editor::Toggle(on) => ToggleSwitch::new()
            .is_on(on)
            .on_toggled(cx.callback(move |v| Msg::Toggle(field, v)))
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
        Editor::TriState(value) => ComboBox::new()
            .items_source(["Keep the box's setting", "On", "Off"])
            .selected_index(Some(match value {
                None => 0,
                Some(true) => 1,
                Some(false) => 2,
            }))
            .on_selection_changed(cx.callback(move |i| Msg::TriState(field, i)))
            .vertical_alignment(VerticalAlignment::Center)
            .into(),
        Editor::Bind(value) => {
            // Four answers, one control pair. The address box stays beside the list rather
            // than appearing on the fourth choice: a row that changes height as you open a
            // dropdown reflows the whole column under the pointer. It is enabled only when it
            // means something, so an address typed there is never quietly ignored.
            let selected = match value.as_deref() {
                None => 0,
                Some(LOOPBACK_BIND) => 1,
                Some(LAN_BIND) => 2,
                Some(_) => 3,
            };
            let typed = match (&value, selected) {
                (Some(addr), 3) => addr.clone(),
                _ => String::new(),
            };
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(6.0)
                .vertical_alignment(VerticalAlignment::Center)
                .children((
                    ComboBox::new()
                        .items_source([
                            "Leave host.env alone",
                            "This PC only",
                            "This local network",
                            "A specific address",
                        ])
                        .selected_index(Some(selected))
                        .on_selection_changed(cx.callback(Msg::BindChoice))
                        .vertical_alignment(VerticalAlignment::Center),
                    TextBox::new()
                        .text(typed)
                        .placeholder_text("e.g. 100.64.0.3")
                        .is_enabled(selected == 3)
                        .min_width(160.0)
                        .on_text_changed(
                            cx.callback(|addr: Rc<str>| Msg::BindAddress(addr.to_string())),
                        ),
                ))
                .into()
        }
        Editor::Password(value) => {
            // The reveal is this toggle's, not WinUI's eye: the eye only appears while the
            // user types, so a pre-generated password would never be revealable.
            let reveal = w.reveal;
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(6.0)
                .vertical_alignment(VerticalAlignment::Center)
                .children((
                    PasswordBox::new()
                        .password(value.unwrap_or_default())
                        .password_reveal_mode(if reveal {
                            PasswordRevealMode::Visible
                        } else {
                            PasswordRevealMode::Hidden
                        })
                        .min_width(240.0)
                        .on_password_changed(
                            cx.callback(|pw: Rc<str>| Msg::Password(pw.to_string())),
                        ),
                    Button::new()
                        .content(if reveal { "Hide" } else { "Show" })
                        .min_width(72.0)
                        .on_click(cx.message(Msg::Reveal(!reveal))),
                    Button::new()
                        .content("Regenerate")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.message(Msg::Regenerate)),
                ))
                .into()
        }
    };
    let words = StackPanel::new().spacing(1.0).children((
        TextBlock::new()
            .text(WinScreen::label(field))
            .font_weight(FontWeight::SEMI_BOLD),
        note(w, w.screen.why(field)).font_size(12.0),
    ));
    // The password and bind editors are wider than a section column's Auto slot leaves room
    // for: side by side, the label column collapses to nothing and the wrapped hint explodes
    // in height. Those rows stack instead.
    if field == Field::Password || field == Field::WebBind {
        return StackPanel::new()
            .spacing(2.0)
            .children((
                words,
                Border::new()
                    .margin(Thickness::new(0.0, 8.0, 0.0, 0.0))
                    .content(editor),
            ))
            .into();
    }
    Grid::new()
        .columns([GridLength::STAR, GridLength::Auto])
        .column_spacing(16.0)
        .children((
            words
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(0),
            Border::new().grid_column(1).content(editor),
        ))
        .into()
}
