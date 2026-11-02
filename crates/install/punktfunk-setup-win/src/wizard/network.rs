//! Network: the bind and exposure answer.

use super::*;

pub(super) fn network_page(w: &Wizard, cx: &ViewContext<'_, Wizard>) -> View {
    let name = w.network_name();
    let selected = match &w.screen.choices.network {
        NetworkAnswer::MakePrivate(_) => 0,
        NetworkAnswer::OpenPublicRules => 1,
        NetworkAnswer::Skip => 2,
    };
    let content = StackPanel::new()
        .spacing(12.0)
        .max_width(640.0)
        .children((
            TextBlock::new()
                .text(format!("'{name}' is set to Public"))
                .font_size(16.0)
                .font_weight(FontWeight::SEMI_BOLD),
            note(
                w,
                "Windows scopes the standard firewall rules to private networks, so on this network the host would be silently unreachable. VPN and virtual adapters are routinely Public by design — this changes only the named network, never a global setting.",
            ),
            RadioButtons::new()
                .items_source([
                    format!("This is my home network — make '{name}' Private (recommended)"),
                    "Keep it Public, open the firewall for it".to_string(),
                    "Skip — I'll fix it later".to_string(),
                ])
                .selected_index(Some(selected))
                .on_selection_changed(cx.callback(Msg::Network)),
        ));
    frame(
        w,
        WizStep::Network,
        content,
        vec![
            back_button(cx, WizStep::Network),
            continue_button(cx, WizStep::Network, "Install"),
        ],
    )
}
