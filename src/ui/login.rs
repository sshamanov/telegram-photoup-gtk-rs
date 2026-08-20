use gtk4::{prelude::*, Box as GBox, Button, Entry, Label, Orientation};
use crate::state::{AppEvent, AppState, AuthEvent, AuthStatus};
use std::sync::{Arc, RwLock};

/// Login screen: phone → code → 2FA, rendered from `AppState::telegram.status`.
///
/// Buttons dispatch `AppEvent::Auth` through `on_event`; the controller (Task 21)
/// forwards those to the Telegram worker and reduces the result into state.
pub struct LoginScreen {
    pub root: GBox,
    step_label: Label,
    phone_entry: Entry,
    code_entry: Entry,
    password_entry: Entry,
    primary: Button,
}

impl LoginScreen {
    pub fn new(state: Arc<RwLock<AppState>>, on_event: Box<dyn Fn(crate::state::AppEvent) + Send + 'static>) -> Self {
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

        // The button emits the event for whatever step is currently visible, read
        // from state so the phone/code/password text goes to the right command.
        let on = on_event;
        let st = Arc::clone(&state);
        let phone = phone_entry.clone();
        let code = code_entry.clone();
        let password = password_entry.clone();
        primary.connect_clicked(move |_| {
            let status = st.read().unwrap().telegram.status.clone();
            match status {
                AuthStatus::AwaitingCode => {
                    on(AppEvent::Auth(AuthEvent::CodeEntered { code: code.text().to_string() }));
                }
                AuthStatus::Awaiting2fa => {
                    on(AppEvent::Auth(AuthEvent::PasswordEntered { password: password.text().to_string() }));
                }
                _ => {
                    on(AppEvent::Auth(AuthEvent::PhoneRequested { phone: phone.text().to_string() }));
                }
            }
        });

        Self { root, step_label, phone_entry, code_entry, password_entry, primary }
    }

    /// Reflect `AuthStatus` in the entry/button/step-label UI. Called by the
    /// controller whenever Telegram events change the auth status.
    pub fn render(&self, status: &AuthStatus) {
        match status {
            AuthStatus::AwaitingCode => {
                self.phone_entry.set_visible(false);
                self.code_entry.set_visible(true);
                self.password_entry.set_visible(false);
                self.primary.set_label("Submit code");
                self.step_label.set_text("Enter the login code sent to your phone");
            }
            AuthStatus::Awaiting2fa => {
                self.phone_entry.set_visible(false);
                self.code_entry.set_visible(false);
                self.password_entry.set_visible(true);
                self.primary.set_label("Submit 2FA password");
                self.step_label.set_text("This account requires a 2FA password");
            }
            AuthStatus::Failed(msg) => {
                self.phone_entry.set_visible(true);
                self.code_entry.set_visible(false);
                self.password_entry.set_visible(false);
                self.primary.set_label("Request code");
                self.step_label.set_text(&format!("Login failed: {msg}"));
            }
            _ => {
                self.phone_entry.set_visible(true);
                self.code_entry.set_visible(false);
                self.password_entry.set_visible(false);
                self.primary.set_label("Request code");
                self.step_label.set_text("Enter your phone number to start");
            }
        }
    }
}
