//! The facade against a fake IMAP server, through Gmail's HTTP API.

mod support;

use std::collections::BTreeSet;

use gmail::Format;
use support::{ACCOUNT, Fake, TOKEN, history, history_id, mail, messages, open, serve};

const T0: i64 = 1_763_370_000_000;

fn labels(list: &[&str]) -> Vec<String> {
    let mut labels: Vec<String> = list.iter().map(|l| (*l).to_owned()).collect();
    labels.sort();
    labels
}

/// An inbox message, its reply in Sent, and one old filed message.
fn seeded() -> Fake {
    let fake = Fake::new();
    fake.deliver(
        "INBOX",
        mail(
            "Priya <priya@example.com>",
            "Launch",
            "a@x",
            None,
            "Ship it?",
        ),
        &[],
        T0,
    );
    fake.deliver(
        "Sent",
        mail(ACCOUNT, "Re: Launch", "b@x", Some("a@x"), "Yes."),
        &["\\Seen"],
        T0 + 60_000,
    );
    fake.deliver(
        "Clients",
        mail("Bo <bo@example.com>", "Invoice", "c@x", None, "Attached."),
        &["\\Seen", "\\Flagged"],
        T0 - 86_400_000,
    );
    fake
}

#[tokio::test]
async fn first_sync_maps_folders_flags_and_threads() {
    let fake = seeded();
    let served = serve(open(&fake, None)).await;
    let report = served.facade.sync().await.unwrap();
    assert_eq!(report.added, 3);

    let all = messages(&served.gmail).await;
    let (_, launch_thread, launch_labels) = &all["Launch"];
    let (_, reply_thread, reply_labels) = &all["Re: Launch"];
    assert_eq!(
        launch_thread, reply_thread,
        "a reply in Sent joins its thread"
    );
    assert_eq!(
        launch_labels,
        &labels(&["CATEGORY_PERSONAL", "INBOX", "UNREAD"])
    );
    assert_eq!(reply_labels, &labels(&["SENT"]));
    let (_, _, invoice_labels) = &all["Invoice"];
    assert!(invoice_labels.contains(&"STARRED".to_owned()));
    assert!(!invoice_labels.contains(&"INBOX".to_owned()));
    // The folder is a user label, named for it.
    let named: Vec<_> = served
        .gmail
        .gmail()
        .labels()
        .await
        .unwrap()
        .labels
        .into_iter()
        .filter(|l| l.name.as_deref() == Some("Clients"))
        .collect();
    assert_eq!(named.len(), 1);
    assert!(invoice_labels.contains(named[0].id.as_ref().unwrap()));

    let thread = served
        .gmail
        .gmail()
        .thread(launch_thread, Format::Minimal, &[])
        .await
        .unwrap();
    assert_eq!(thread.messages.len(), 2);
    let profile = served.gmail.gmail().profile().await.unwrap();
    assert_eq!(profile.messages_total, Some(3));
    assert_eq!(profile.email_address.as_deref(), Some(ACCOUNT));
}

#[tokio::test]
async fn changes_made_elsewhere_arrive_with_the_same_ids() {
    let fake = seeded();
    let served = serve(open(&fake, None)).await;
    served.facade.sync().await.unwrap();
    let before = messages(&served.gmail).await;
    let start = history_id(&served.gmail).await;

    // Read in webmail, starred on a phone, filed by a rule, and mail from someone new.
    fake.set_flags("INBOX", 1, &["\\Seen", "\\Flagged"]);
    fake.mv("Clients", 1, "Archive");
    fake.deliver(
        "INBOX",
        mail("New <new@example.com>", "Hello", "d@x", None, "Hi"),
        &[],
        T0 + 1,
    );
    let report = served.facade.sync().await.unwrap();
    assert_eq!(
        (report.added, report.moved, report.flagged, report.deleted),
        (1, 1, 1, 0)
    );

    let after = messages(&served.gmail).await;
    assert_eq!(
        after["Launch"].0, before["Launch"].0,
        "same message, same ID"
    );
    assert_eq!(
        after["Launch"].2,
        labels(&["CATEGORY_PERSONAL", "INBOX", "STARRED"])
    );
    assert_eq!(
        after["Invoice"].0, before["Invoice"].0,
        "a move is not a delete and an add"
    );
    let clients = served
        .gmail
        .gmail()
        .labels()
        .await
        .unwrap()
        .labels
        .into_iter()
        .find(|l| l.name.as_deref() == Some("Clients"))
        .and_then(|l| l.id)
        .unwrap();
    assert!(before["Invoice"].2.contains(&clients));
    assert!(
        !after["Invoice"].2.contains(&clients),
        "the move took the label away"
    );

    let changes = history(&served.gmail, start)
        .await
        .expect("history is served");
    let kinds: BTreeSet<_> = changes.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, BTreeSet::from(["added", "labels+", "labels-"]));
    assert!(!changes.iter().any(|(kind, _)| *kind == "deleted"));
}

#[tokio::test]
async fn expunged_mail_is_deleted() {
    let fake = seeded();
    let served = serve(open(&fake, None)).await;
    served.facade.sync().await.unwrap();
    let start = history_id(&served.gmail).await;
    fake.expunge("INBOX", 1);
    assert_eq!(served.facade.sync().await.unwrap().deleted, 1);
    assert!(!messages(&served.gmail).await.contains_key("Launch"));
    let changes = history(&served.gmail, start).await.unwrap();
    assert!(changes.iter().any(|(kind, _)| *kind == "deleted"));
}

#[tokio::test]
async fn a_renumbered_folder_keeps_its_messages() {
    let fake = seeded();
    let served = serve(open(&fake, None)).await;
    served.facade.sync().await.unwrap();
    let before = messages(&served.gmail).await;
    fake.renumber("INBOX");
    let report = served.facade.sync().await.unwrap();
    assert_eq!((report.added, report.deleted), (0, 0));
    assert_eq!(messages(&served.gmail).await, before);
    // And the new UIDs are the ones used from here on.
    fake.set_flags("INBOX", 100, &["\\Seen"]);
    assert_eq!(served.facade.sync().await.unwrap().flagged, 1);
}

#[tokio::test]
async fn works_without_condstore() {
    let fake = seeded();
    fake.server().condstore = false;
    let served = serve(open(&fake, None)).await;
    served.facade.sync().await.unwrap();
    fake.set_flags("INBOX", 1, &["\\Seen"]);
    assert_eq!(served.facade.sync().await.unwrap().flagged, 1);
    assert_eq!(served.facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
async fn ids_and_history_survive_a_restart() {
    let fake = seeded();
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("kahlus.imap.sqlite3");

    let first = serve(open(&fake, Some(&index))).await;
    first.facade.sync().await.unwrap();
    fake.set_flags("INBOX", 1, &["\\Seen"]);
    first.facade.sync().await.unwrap();
    let before = messages(&first.gmail).await;
    let cursor = history_id(&first.gmail).await;
    // Mail that arrives while Exo is closed.
    fake.deliver(
        "INBOX",
        mail("Late <late@example.com>", "Later", "e@x", None, "Hi"),
        &[],
        T0 + 5,
    );
    drop(first);

    let second = serve(open(&fake, Some(&index))).await;
    // Before any contact with the server, the mailbox is what it was.
    assert_eq!(messages(&second.gmail).await, before);
    assert_eq!(second.facade.engine.with(|m| m.history.current()), cursor);
    let changes = history(&second.gmail, cursor)
        .await
        .expect("no rebuild after a restart");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0, "added");
    let after = messages(&second.gmail).await;
    assert_eq!(after["Launch"], before["Launch"]);
    assert!(after.contains_key("Later"));
}

#[tokio::test]
async fn text_is_fetched_when_asked_for() {
    let fake = seeded();
    let served = serve(open(&fake, None)).await;
    served.facade.sync().await.unwrap();
    let id = messages(&served.gmail).await["Launch"].0.clone();
    assert_eq!(fake.server().body_fetches, 0, "listing needs headers only");

    let message = served
        .gmail
        .gmail()
        .message(&id, Format::Full, &[])
        .await
        .unwrap();
    let text = message.payload.unwrap().body.unwrap().data.unwrap();
    assert_eq!(String::from_utf8(text).unwrap().trim(), "Ship it?");
    assert_eq!(fake.server().body_fetches, 1);
    served
        .gmail
        .gmail()
        .message(&id, Format::Full, &[])
        .await
        .unwrap();
    assert_eq!(fake.server().body_fetches, 1, "once is enough");

    let thread_id = messages(&served.gmail).await["Launch"].1.clone();
    served
        .gmail
        .gmail()
        .thread(&thread_id, Format::Full, &[])
        .await
        .unwrap();
    assert_eq!(
        fake.server().body_fetches,
        2,
        "the reply's text came with its thread"
    );
}

#[tokio::test]
async fn a_server_that_is_down_leaves_the_mailbox_alone() {
    let fake = seeded();
    let served = serve(open(&fake, None)).await;
    served.facade.sync().await.unwrap();
    let before = messages(&served.gmail).await;
    fake.deliver(
        "INBOX",
        mail("New <new@example.com>", "Hello", "d@x", None, "Hi"),
        &[],
        T0 + 1,
    );
    fake.server().down = true;
    assert!(served.facade.sync().await.is_err());
    // History still answers, from what is indexed.
    assert_eq!(messages(&served.gmail).await, before);
    fake.server().down = false;
    assert_eq!(served.facade.sync().await.unwrap().added, 1);
}

#[tokio::test]
async fn only_the_app_may_use_it_and_only_for_mail() {
    let fake = seeded();
    let served = serve(open(&fake, None)).await;
    served.facade.sync().await.unwrap();
    let http = reqwest::Client::new();
    let url = format!("{}/gmail/v1/users/me/profile", served.origin);

    let status = |request: reqwest::RequestBuilder| async move {
        request.send().await.unwrap().status().as_u16()
    };
    assert_eq!(status(http.get(&url)).await, 401, "no token");
    assert_eq!(
        status(http.get(&url).bearer_auth("guess")).await,
        401,
        "wrong token"
    );
    assert_eq!(status(http.get(&url).bearer_auth(TOKEN)).await, 200);
    assert_eq!(
        status(
            http.get(&url)
                .bearer_auth(TOKEN)
                .header("Origin", "http://evil.example")
        )
        .await,
        403,
        "a web page"
    );
    // Mail changes are the app's to make; settings and labels are not.
    let filters = format!("{}/gmail/v1/users/me/settings/filters", served.origin);
    assert_eq!(
        status(http.post(&filters).bearer_auth(TOKEN).body("{}")).await,
        403,
        "filters"
    );
    let label = format!("{}/gmail/v1/users/me/labels", served.origin);
    assert_eq!(
        status(http.post(&label).bearer_auth(TOKEN).body("{}")).await,
        403
    );
    assert_eq!(
        status(http.post(&filters).body("{}")).await,
        401,
        "no token"
    );
    let admin = format!("{}/__mock/reset", served.origin);
    assert_eq!(status(http.post(&admin).bearer_auth(TOKEN)).await, 403);
    assert_eq!(
        status(
            http.get(format!("{}/__mock/state", served.origin))
                .bearer_auth(TOKEN)
        )
        .await,
        404
    );
}
