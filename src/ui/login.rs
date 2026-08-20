use gtk4::{prelude::*, Box as GBox, Button, Entry, Label, Orientation};
use crate::state::AppState;
use std::sync::{Arc, RwLock};

/// Login screen: phone → code → 2FA, rendered from `AppState::telegram.status`.
///
/// Field handles (step_label, entries, primary) are kept for Task 21 to switch
/// steps from `AuthStatus`; they are not read yet, hence `#[allow(dead_code)]`.
#[allow(dead_code)]
pub struct LoginScreen {
    pub root: GBox,
    step_label: Label,
    phone_entry: Entry,
    code_entry: Entry,
    password_entry: Entry,
    primary: Button,
}

impl LoginScreen {
    pub fn new(_state: Arc<RwLock<AppState>>, on_event: Box<dyn Fn(crate::state::AppEvent) + Send + 'static>) -> Self {
        let root = GBox::new(Orientation::Vertical, 12);
        root.set_margin_top(40);
        root.set_margin_bottom(40);
        root.set_margin_start(60);
        root.set_margin_end(60);
        root.set_valign(gtk4::Align::Center);
        root.set_halign(gtk4::Align::Center);
        root.set_width_request(360);

        let title = Label::new(Some("photoup2 — Telegram login"));
        title.add_css_class("title-1");
        root.append(&title);

        let step_label = Label::new(Some("Enter your phone number to start"));
        root.append(&step_label);

        let phone_entry = Entry::new();
        phone_entry.set_placeholder_text(Some("+1 555 0132"));
        root.append(&phone_entry);

        let code_entry = Entry::new();
        code_entry.set_placeholder_text(Some("Login code"));
        code_entry.set_visible(false);
        root.append(&code_entry);

        let password_entry = Entry::new();
        password_entry.set_placeholder_text(Some("2FA password"));
        password_entry.set_visibility(false);
        password_entry.set_visible(false);
        root.append(&password_entry);

        let primary = Button::with_label("Request code");
        primary.add_css_class("suggested-action");
        root.append(&primary);

        let on = on_event;
        // Clone so the closure can own it while the struct keeps its own handle.
        let phone_entry_for_click = phone_entry.clone();
        primary.connect_clicked(move |_| {
            let _phone = phone_entry_for_click.text().to_string();
            on(crate::state::AppEvent::Auth(crate::state::AuthEvent::PhoneRequested));
        });

        Self { root, step_label, phone_entry, code_entry, password_entry, primary }
    }
}
