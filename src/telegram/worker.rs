//! The Telegram IO thread: owns grammers + a tokio current-thread runtime.
//! Commands arrive via channel from the UI; events go back via channel to the
//! GTK main loop. Never blocks on image work. This is mpd-client's "MPD IO thread".
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use super::grammers::GrammersSession;
use super::{AuthStep, TCommand, TEvent};

pub struct TelegramWorkerConfig {
    pub session_path: PathBuf,
    pub api_id: i32,
    pub api_hash: String,
}

/// Spawn the worker thread. Returns the command sender.
pub fn spawn(
    cfg: TelegramWorkerConfig,
    events: Sender<TEvent>,
) -> std::sync::mpsc::Sender<TCommand> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TCommand>();
    std::thread::Builder::new()
        .name("telegram-io".into())
        .spawn(move || run(cfg, cmd_rx, events))
        .expect("spawn telegram thread");
    cmd_tx
}

fn run(cfg: TelegramWorkerConfig, cmds: Receiver<TCommand>, events: Sender<TEvent>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");
    rt.block_on(async move {
        // Ensure the session dir exists (SqliteSession::open fails if the parent is missing).
        if let Some(dir) = cfg.session_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut connect_err: Option<String> = None;
        let mut sess = match GrammersSession::connect(&cfg.session_path, cfg.api_id).await {
            Ok(s) => Some(s),
            Err(e) => {
                let msg = format!("connect: {e}");
                let _ = events.send(TEvent::Error(msg.clone()));
                connect_err = Some(msg);
                None
            }
        };
        // 2FA password token captured between SubmitCode (PasswordRequired) and SubmitPassword.
        let mut pending_password: Option<grammers_client::client::PasswordToken> = None;
        for cmd in cmds.iter() {
            // Survive a failed connect: keep the thread alive and surface the error on
            // every command so the UI isn't a dead channel.
            let Some(session) = sess.as_mut() else {
                if let Some(err) = &connect_err {
                    let _ = events.send(TEvent::Error(err.clone()));
                }
                continue;
            };
            match cmd {
                TCommand::CheckAuth => {
                    let ok = session.is_authorized().await;
                    let _ = events.send(TEvent::AuthStep(if ok { AuthStep::Ready } else { AuthStep::NotStarted }));
                }
                TCommand::RequestCode { phone } => {
                    match session.request_code(&phone, &cfg.api_hash).await {
                        Ok(()) => { let _ = events.send(TEvent::CodeRequested); }
                        Err(e) => { let _ = events.send(TEvent::AuthFailed(e)); }
                    }
                }
                TCommand::SubmitCode { code } => {
                    use super::grammers::SignInOutcome;
                    match session.sign_in(&code).await {
                        Ok(()) => { let _ = events.send(TEvent::AuthStep(AuthStep::Ready)); }
                        Err(SignInOutcome::PasswordRequired(pw)) => {
                            let hint = pw.hint().map(|s| s.to_string());
                            let _ = events.send(TEvent::PasswordRequired { hint });
                            pending_password = Some(pw);
                        }
                        Err(SignInOutcome::Failure(msg)) => { let _ = events.send(TEvent::AuthFailed(msg)); }
                        Err(SignInOutcome::Ok) => unreachable!("sign_in never returns SignInOutcome::Ok as an error"),
                    }
                }
                TCommand::SubmitPassword { password } => {
                    match pending_password.take() {
                        Some(pw) => match session.complete_2fa(pw, &password).await {
                            Ok(()) => { let _ = events.send(TEvent::AuthStep(AuthStep::Ready)); }
                            Err(e) => { let _ = events.send(TEvent::AuthFailed(e)); }
                        },
                        None => { let _ = events.send(TEvent::AuthFailed("no pending 2fa".into())); }
                    }
                }
                TCommand::LoadDialogs => match session.load_dialogs().await {
                    Ok(d) => { let _ = events.send(TEvent::Dialogs(d)); }
                    Err(e) => { let _ = events.send(TEvent::Error(e)); }
                },
                TCommand::SendPhoto { peer_id, access_hash, path, caption } => {
                    match session.send_album(peer_id, access_hash, std::slice::from_ref(&path), caption.as_deref()).await {
                        Ok(_) => { let _ = events.send(TEvent::Sent { ok: 1, failed: vec![] }); }
                        Err(e) => { let _ = events.send(TEvent::Error(e)); }
                    }
                }
                TCommand::SendAlbum { peer_id, access_hash, paths, caption } => {
                    match session.send_album(peer_id, access_hash, &paths, caption.as_deref()).await {
                        Ok(_) => { let _ = events.send(TEvent::Sent { ok: paths.len(), failed: vec![] }); }
                        Err(e) => { let _ = events.send(TEvent::Error(e)); }
                    }
                }
            }
        }
    });
}
