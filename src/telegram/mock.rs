//! Deterministic mock adapter for tests and offline dev (mpd-client's mock MPD server).
use super::{AuthStep, DialogInfo, TCommand, TEvent, TelegramAdapter};
use std::path::PathBuf;

pub struct MockAdapter {
    pub auth: AuthStep,
    pub sent_album: Option<Vec<PathBuf>>,
}

impl MockAdapter {
    pub fn new() -> Self {
        Self { auth: AuthStep::NotStarted, sent_album: None }
    }
}

impl TelegramAdapter for MockAdapter {
    fn handle(&mut self, cmd: TCommand) -> Vec<TEvent> {
        match cmd {
            TCommand::RequestCode { .. } => {
                self.auth = AuthStep::AwaitingCode;
                vec![TEvent::CodeRequested]
            }
            TCommand::SubmitCode { code } => {
                if code == "12345" {
                    self.auth = AuthStep::AwaitingPassword;
                    vec![TEvent::PasswordRequired { hint: Some("hint".into()) }]
                } else if code == "12346" {
                    self.auth = AuthStep::Ready;
                    vec![TEvent::AuthStep(AuthStep::Ready)]
                } else {
                    vec![TEvent::AuthFailed("invalid code".into())]
                }
            }
            TCommand::SubmitPassword { password } => {
                if password == "hunter2" {
                    self.auth = AuthStep::Ready;
                    vec![TEvent::AuthStep(AuthStep::Ready)]
                } else {
                    vec![TEvent::AuthFailed("bad password".into())]
                }
            }
            TCommand::LoadDialogs => {
                vec![TEvent::Dialogs(vec![
                    DialogInfo { id: 1, title: "My Group".into(), is_group: true, access_hash: Some(1) },
                    DialogInfo { id: 2, title: "Test Group".into(), is_group: true, access_hash: Some(1) },
                ])]
            }
            TCommand::SendPhoto { .. } => {
                vec![TEvent::Sent { ok: 1, failed: vec![] }]
            }
            TCommand::SendAlbum { paths, .. } => {
                self.sent_album = Some(paths.clone());
                vec![TEvent::Sent { ok: paths.len(), failed: vec![] }]
            }
            TCommand::CheckAuth => {
                vec![TEvent::AuthStep(self.auth.clone())]
            }
        }
    }
}
