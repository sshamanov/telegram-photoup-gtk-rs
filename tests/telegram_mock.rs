use photoup2::telegram::mock::MockAdapter;
use photoup2::telegram::{AuthStep, TCommand, TelegramAdapter};

#[test]
fn full_login_flow() {
    let mut tg = MockAdapter::new();
    assert!(matches!(tg.handle(TCommand::CheckAuth)[0], photoup2::telegram::TEvent::AuthStep(AuthStep::NotStarted)));
    assert!(matches!(tg.handle(TCommand::RequestCode { phone: "+1".into() })[0], photoup2::telegram::TEvent::CodeRequested));
    let evs = tg.handle(TCommand::SubmitCode { code: "12345".into() });
    assert!(matches!(&evs[0], photoup2::telegram::TEvent::PasswordRequired { .. }));
    let evs = tg.handle(TCommand::SubmitPassword { password: "hunter2".into() });
    assert!(matches!(&evs[0], photoup2::telegram::TEvent::AuthStep(AuthStep::Ready)));
}

#[test]
fn dialogs_and_album_send() {
    let mut tg = MockAdapter::new();
    let dialogs = tg.handle(TCommand::LoadDialogs);
    assert!(matches!(&dialogs[0], photoup2::telegram::TEvent::Dialogs(d) if d.len() == 2));
    let paths = vec![std::path::PathBuf::from("/a.jpg"), std::path::PathBuf::from("/b.jpg")];
    let evs = tg.handle(TCommand::SendAlbum { peer_id: 1, paths: paths.clone(), caption: None });
    assert!(matches!(&evs[0], photoup2::telegram::TEvent::Sent { ok: 2, .. }));
    assert_eq!(tg.sent_album.as_ref().unwrap().len(), 2);
}
