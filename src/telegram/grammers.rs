use std::path::PathBuf;
use std::sync::Arc;

use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::media::InputMedia;
use grammers_client::peer::Peer;
use grammers_client::{Client, SenderPool, SignInError};
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerAuth, PeerId, PeerRef};

use super::{DialogInfo, SentResult};

/// Outcome of a code submission — lets the worker ask for a 2FA password.
pub enum SignInOutcome {
    Ok,
    PasswordRequired(PasswordToken),
    Failure(String),
}

/// Holds the (thread-confined) grammers client. `run` owns this on the worker thread.
pub struct GrammersSession {
    client: Client,
    /// Set by `request_login_code`, consumed by `sign_in` (grammers 0.10 keeps the
    /// token client-side between the two calls).
    login_token: Option<LoginToken>,
}

impl GrammersSession {
    /// Build a SenderPool + Client from a sqlite session file. `api_id` comes from
    /// config (my.telegram.org app credentials).
    pub async fn connect(session_path: &PathBuf, api_id: i32) -> Result<Self, String> {
        let session = Arc::new(SqliteSession::open(session_path).await.map_err(|e| e.to_string())?);
        let pool = SenderPool::new(Arc::clone(&session), api_id);
        let client = Client::new(pool.handle);
        // Drive I/O on a background tokio task of this runtime.
        tokio::spawn(pool.runner.run());
        Ok(Self { client, login_token: None })
    }

    pub async fn is_authorized(&self) -> bool {
        self.client.is_authorized().await.unwrap_or(false)
    }

    /// Request the SMS code; stores the returned `LoginToken` for the `sign_in` step.
    pub async fn request_code(&mut self, phone: &str, api_hash: &str) -> Result<(), String> {
        let token = self.client.request_login_code(phone, api_hash).await.map_err(|e| e.to_string())?;
        self.login_token = Some(token);
        Ok(())
    }

    /// Submit the login code. On 2FA, returns `PasswordRequired` so the worker can
    /// ask the user for the password (photoup's flow).
    pub async fn sign_in(&mut self, code: &str) -> Result<(), SignInOutcome> {
        let Some(token) = self.login_token.take() else {
            return Err(SignInOutcome::Failure("no login token".into()));
        };
        match self.client.sign_in(&token, code).await {
            Ok(_user) => Ok(()),
            Err(SignInError::PasswordRequired(pw)) => Err(SignInOutcome::PasswordRequired(pw)),
            Err(SignInError::InvalidCode) => Err(SignInOutcome::Failure("invalid code".into())),
            Err(e) => Err(SignInOutcome::Failure(e.to_string())),
        }
    }

    pub async fn complete_2fa(&self, pw: PasswordToken, password: &str) -> Result<(), String> {
        match self.client.check_password(pw, password).await {
            Ok(_user) => Ok(()),
            Err(SignInError::InvalidPassword(_)) => Err("wrong 2FA password".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub async fn load_dialogs(&self) -> Result<Vec<DialogInfo>, String> {
        let mut it = self.client.iter_dialogs();
        let mut out = Vec::new();
        while let Some(dialog) = it.next().await.map_err(|e| e.to_string())? {
            out.push(peer_to_dialog(dialog.peer()).await?);
            if out.len() >= 200 { break; }
        }
        Ok(out)
    }

    /// Upload each path and send them as a single album (or a single photo).
    pub async fn send_album(&self, peer_id: i64, access_hash: Option<i64>, paths: &[PathBuf], caption: Option<&str>) -> Result<SentResult, String> {
        let peer_id = PeerId::from_bot_api_dialog_id(peer_id)
            .ok_or_else(|| format!("invalid peer id {peer_id}"))?;
        let auth = access_hash.map(PeerAuth::from_hash).unwrap_or_default();
        let peer = PeerRef { id: peer_id, auth };
        let mut medias = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let uploaded = self.client.upload_file(p).await.map_err(|e| e.to_string())?;
            let mut media = InputMedia::new().photo(uploaded);
            if i == 0 {
                if let Some(c) = caption {
                    media = media.caption(c);
                }
            }
            medias.push(media);
        }
        let messages = self.client.send_album(peer, medias).await.map_err(|e| e.to_string())?;
        let ids = messages.iter().flatten().map(|m| m.id()).collect();
        Ok(SentResult { message_ids: ids })
    }
}

/// Extract a `DialogInfo` from a freshly-fetched dialog peer.
///
/// The id is stored in Bot-API tagged form (embeds the peer kind) so `send_album` can
/// rebuild a `PeerRef`. The access hash comes from the peer's cached auth; `None` when
/// the hash is unavailable (e.g. a minimal user or a plain basic group).
async fn peer_to_dialog(peer: &Peer) -> Result<DialogInfo, String> {
    let id = peer.id().bot_api_dialog_id_unchecked();
    let title = peer.name().unwrap_or("").to_string();
    let is_group = matches!(peer, Peer::Group(_) | Peer::Channel(_));
    let access_hash = peer.to_ref().await.ok().flatten().map(|pr| pr.auth.hash());
    Ok(DialogInfo { id, title, is_group, access_hash })
}
