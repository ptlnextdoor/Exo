//! Changes Exo makes reach the server: flags, folders, deletes and sent mail, through Gmail's
//! HTTP API, against the fake server.

mod support;

use gmail::{
    BatchModifyMessagesRequest, Format, ListQuery, ModifyMessageRequest, ModifyThreadRequest,
};
use support::{ACCOUNT, Fake, FakeStore, Served, history, history_id, mail, messages, open, serve};

const T0: i64 = 1_763_370_000_000;

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|l| (*l).to_owned()).collect()
}

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
        mail(ACCOUNT, "Thanks", "b@x", None, "Yes."),
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

fn count(fake: &Fake, folder: &str) -> usize {
    fake.server().folders[folder].messages.len()
}

fn flags(fake: &Fake, folder: &str) -> Vec<Vec<String>> {
    let mut all: Vec<_> = fake.server().folders[folder]
        .messages
        .values()
        .map(|m| {
            let mut flags = m.flags.clone();
            flags.sort();
            flags
        })
        .collect();
    all.sort();
    all
}

async fn modify(served: &Served, id: &str, add: &[&str], remove: &[&str]) -> bool {
    served
        .gmail
        .gmail()
        .modify_message(
            id,
            &ModifyMessageRequest {
                add_label_ids: ids(add),
                remove_label_ids: ids(remove),
                ..ModifyMessageRequest::default()
            },
        )
        .await
        .is_ok()
}

async fn synced(fake: &Fake) -> Served {
    let served = serve(open(fake, None)).await;
    served.facade.sync().await.unwrap();
    served
}

#[tokio::test]
async fn archive_star_and_read_reach_the_server() {
    let fake = seeded();
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("index.sqlite3");
    let served = serve(open(&fake, Some(&index))).await;
    served.facade.sync().await.unwrap();
    let before = messages(&served.gmail).await;
    let start = history_id(&served.gmail).await;
    let (id, thread) = (before["Launch"].0.clone(), before["Launch"].1.clone());

    // Done, starred and read, as Exo does it to a thread.
    served
        .gmail
        .gmail()
        .modify_thread(
            &thread,
            &ModifyThreadRequest {
                add_label_ids: ids(&["STARRED"]),
                remove_label_ids: ids(&["INBOX", "UNREAD"]),
            },
        )
        .await
        .unwrap();
    assert_eq!(count(&fake, "INBOX"), 0);
    assert_eq!(flags(&fake, "Archive"), [["\\Flagged", "\\Seen"]]);

    let after = messages(&served.gmail).await;
    assert_eq!(after["Launch"].0, id, "same message, same ID");
    assert!(after["Launch"].2.contains(&"STARRED".to_owned()));
    assert!(!after["Launch"].2.contains(&"INBOX".to_owned()));
    let changes = history(&served.gmail, start).await.unwrap();
    assert!(changes.contains(&("labels-", id.clone())), "{changes:?}");
    assert!(changes.contains(&("labels+", id.clone())), "{changes:?}");

    // The next pass finds the server as the change left it, and has nothing to add.
    assert_eq!(served.facade.sync().await.unwrap(), Default::default());
    assert_eq!(messages(&served.gmail).await, after);

    // And it was kept: a restart still has the message where Exo put it.
    drop(served);
    let again = serve(open(&fake, Some(&index))).await;
    let restarted = messages(&again.gmail).await;
    assert_eq!(restarted["Launch"].0, id);
    assert!(restarted["Launch"].2.contains(&"STARRED".to_owned()));
    assert!(!restarted["Launch"].2.contains(&"INBOX".to_owned()));
    assert_eq!(again.facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
async fn changes_work_on_a_server_with_no_condstore_too() {
    let fake = seeded();
    fake.server().condstore = false;
    let served = synced(&fake).await;
    let id = messages(&served.gmail).await["Launch"].0.clone();
    assert!(modify(&served, &id, &[], &["UNREAD"]).await);
    assert_eq!(flags(&fake, "INBOX"), [["\\Seen"]]);
    assert_eq!(served.facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
async fn trash_untrash_and_a_label_are_folders() {
    let fake = seeded();
    let served = synced(&fake).await;
    let all = messages(&served.gmail).await;
    let id = all["Launch"].0.clone();
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

    served.gmail.gmail().trash_message(&id).await.unwrap();
    assert_eq!((count(&fake, "INBOX"), count(&fake, "Trash")), (0, 1));
    assert!(
        messages(&served.gmail).await["Launch"]
            .2
            .contains(&"TRASH".to_owned())
    );

    // Gmail leaves an untrashed message out of the inbox, so it is archived.
    served.gmail.gmail().untrash_message(&id).await.unwrap();
    assert_eq!((count(&fake, "Trash"), count(&fake, "Archive")), (0, 1));

    // Back to the inbox, then into a folder by label.
    assert!(modify(&served, &id, &["INBOX"], &[]).await);
    assert_eq!(count(&fake, "INBOX"), 1);
    // Two places at once is more than one IMAP folder can be.
    assert!(!modify(&served, &id, &[&clients], &[]).await);
    assert_eq!(count(&fake, "INBOX"), 1);
    assert!(modify(&served, &id, &[&clients], &["INBOX"]).await);
    assert_eq!((count(&fake, "INBOX"), count(&fake, "Clients")), (0, 2));
    assert!(messages(&served.gmail).await["Launch"].2.contains(&clients));
    assert_eq!(served.facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
async fn a_batch_changes_every_message_once() {
    let fake = seeded();
    fake.deliver(
        "INBOX",
        mail("Cy <cy@example.com>", "Second", "d@x", None, "Hello"),
        &[],
        T0 + 1,
    );
    let served = synced(&fake).await;
    let all = messages(&served.gmail).await;
    served
        .gmail
        .gmail()
        .batch_modify_messages(&BatchModifyMessagesRequest {
            ids: vec![all["Launch"].0.clone(), all["Second"].0.clone()],
            remove_label_ids: ids(&["UNREAD"]),
            ..BatchModifyMessagesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(flags(&fake, "INBOX"), [["\\Seen"], ["\\Seen"]]);
    assert_eq!(served.facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
async fn delete_removes_the_message_for_good() {
    let fake = seeded();
    let served = synced(&fake).await;
    let id = messages(&served.gmail).await["Launch"].0.clone();
    served.gmail.gmail().trash_message(&id).await.unwrap();
    served.gmail.gmail().delete_message(&id).await.unwrap();
    assert_eq!((count(&fake, "INBOX"), count(&fake, "Trash")), (0, 0));
    assert!(!messages(&served.gmail).await.contains_key("Launch"));
    assert_eq!(served.facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
async fn a_server_that_refuses_leaves_everything_as_it_was() {
    let fake = seeded();
    let served = synced(&fake).await;
    let before = messages(&served.gmail).await;
    let id = before["Launch"].0.clone();

    fake.server().down = true;
    assert!(!modify(&served, &id, &[], &["INBOX"]).await);
    assert!(served.gmail.gmail().trash_message(&id).await.is_err());
    fake.server().down = false;
    assert_eq!(
        messages(&served.gmail).await,
        before,
        "the engine did not move"
    );
    assert_eq!(count(&fake, "INBOX"), 1);

    assert!(
        modify(&served, &id, &[], &["INBOX"]).await,
        "and the retry works"
    );
    assert_eq!(count(&fake, "Archive"), 1);
}

/// An outgoing message, in reply to the seeded "Launch".
fn reply() -> Vec<u8> {
    format!(
        "From: {ACCOUNT}\r\nTo: Priya <priya@example.com>\r\nCc: cc@example.com\r\n\
         Bcc: hidden@example.com\r\nSubject: Re: Launch\r\nMessage-ID: <out1@x>\r\n\
         In-Reply-To: <a@x>\r\nReferences: <a@x>\r\nDate: Tue, 17 Nov 2026 10:00:00 +0000\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nGo.\r\n"
    )
    .into_bytes()
}

#[tokio::test]
async fn a_sent_message_goes_out_once_and_is_filed_in_sent() {
    let fake = seeded();
    let served = synced(&fake).await;
    let thread = messages(&served.gmail).await["Launch"].1.clone();

    let sent = served
        .gmail
        .gmail()
        .send(reply(), Some(&thread))
        .await
        .unwrap();
    assert_eq!(sent.label_ids, ["SENT"]);
    assert_eq!(sent.thread_id.as_deref(), Some(thread.as_str()));

    {
        let server = fake.server();
        assert_eq!(server.outbox.len(), 1);
        let (sender, recipients, raw) = &server.outbox[0];
        assert_eq!(sender, ACCOUNT);
        let mut recipients = recipients.clone();
        recipients.sort();
        assert_eq!(
            recipients,
            ["cc@example.com", "hidden@example.com", "priya@example.com"]
        );
        let sent_text = String::from_utf8_lossy(raw).to_lowercase();
        assert!(
            !sent_text.contains("bcc:"),
            "recipients are not told of Bcc"
        );
        assert!(sent_text.contains("go."));
        // The copy in Sent is the author's, with the Bcc and read.
        let copies: Vec<_> = server.folders["Sent"]
            .messages
            .values()
            .filter(|m| String::from_utf8_lossy(&m.raw).contains("out1@x"))
            .collect();
        assert_eq!(copies.len(), 1);
        assert!(String::from_utf8_lossy(&copies[0].raw).contains("Bcc: hidden@example.com"));
        assert!(copies[0].flags.contains(&"\\Seen".to_owned()));
    }

    // Syncing finds that copy and knows it: no second message, nothing moved.
    assert_eq!(served.facade.sync().await.unwrap(), Default::default());
    let all = messages(&served.gmail).await;
    assert_eq!(all["Re: Launch"].0, sent.id.clone().unwrap());
    assert_eq!(all.len(), 4, "{all:?}");

    // Exo's recovery after a crash asks for the message by its Message-ID.
    let found = served
        .gmail
        .gmail()
        .messages(&ListQuery {
            q: Some("rfc822msgid:out1@x".into()),
            ..ListQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(found.messages.len(), 1);
}

#[tokio::test]
async fn a_send_the_server_refuses_leaves_the_draft_to_try_again() {
    let fake = seeded();
    let served = synced(&fake).await;
    let gmail = served.gmail.gmail();

    let draft = gmail.create_draft(reply(), None).await.unwrap();
    let draft_id = draft.id.clone().unwrap();
    assert_eq!(count(&fake, "Drafts"), 0, "drafts stay in the facade");
    let with_draft = messages(&served.gmail).await.len();

    fake.server().smtp_down = true;
    assert!(gmail.send_draft(&draft_id).await.is_err());
    assert!(gmail.send(reply(), None).await.is_err());
    assert!(fake.server().outbox.is_empty());
    assert_eq!(count(&fake, "Sent"), 1, "nothing was filed");
    assert_eq!(
        messages(&served.gmail).await.len(),
        with_draft,
        "and no message appeared"
    );
    gmail.draft(&draft_id, Format::Minimal).await.unwrap();

    fake.server().smtp_down = false;
    let sent = gmail.send_draft(&draft_id).await.unwrap();
    assert_eq!(sent.label_ids, ["SENT"]);
    assert_eq!(fake.server().outbox.len(), 1);
    assert!(gmail.draft(&draft_id, Format::Minimal).await.is_err());
    assert_eq!(count(&fake, "Sent"), 2);
}

#[tokio::test]
async fn a_crash_between_sending_and_filing_does_not_send_twice() {
    let fake = seeded();
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("index.sqlite3");
    let served = serve(open(&fake, Some(&index))).await;
    served.facade.sync().await.unwrap();

    // The outgoing server takes it, then the connection to the mailbox is gone.
    fake.server().down = true;
    let sent = served.gmail.gmail().send(reply(), None).await.unwrap();
    assert_eq!(fake.server().outbox.len(), 1);
    assert_eq!(count(&fake, "Sent"), 1, "not filed yet");
    drop(served);
    fake.server().down = false;

    // On the next start the message is still Exo's, and gets filed, once.
    let again = serve(open(&fake, Some(&index))).await;
    let all = messages(&again.gmail).await;
    assert_eq!(all["Re: Launch"].0, sent.id.clone().unwrap());
    again.facade.sync().await.unwrap();
    assert_eq!(count(&fake, "Sent"), 2);
    assert_eq!(fake.server().outbox.len(), 1);
    assert_eq!(again.facade.sync().await.unwrap(), Default::default());
    assert_eq!(messages(&again.gmail).await.len(), 4);
}

#[tokio::test]
async fn a_copy_the_server_filed_itself_is_not_filed_again() {
    let fake = seeded();
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("index.sqlite3");
    let served = serve(open(&fake, Some(&index))).await;
    served.facade.sync().await.unwrap();
    fake.server().down = true;
    served.gmail.gmail().send(reply(), None).await.unwrap();
    drop(served);
    fake.server().down = false;
    // Another client (or the server) saved the sent message in the meantime.
    fake.deliver("Sent", reply(), &["\\Seen"], T0 + 120_000);

    let again = serve(open(&fake, Some(&index))).await;
    again.facade.sync().await.unwrap();
    assert_eq!(count(&fake, "Sent"), 2);
    assert_eq!(messages(&again.gmail).await.len(), 4);
    assert_eq!(again.facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
async fn search_finds_words_only_the_server_knows() {
    let fake = seeded();
    fake.deliver(
        "Clients",
        mail(
            "Di <di@example.com>",
            "Zoo trip",
            "e@x",
            None,
            "We saw a zebra.",
        ),
        &["\\Seen"],
        T0 - 1,
    );
    let served = synced(&fake).await;
    let find = |q: &str| {
        let gmail = served.gmail.clone();
        let q = q.to_owned();
        async move {
            gmail
                .gmail()
                .messages(&ListQuery {
                    q: Some(q),
                    include_spam_trash: true,
                    ..ListQuery::default()
                })
                .await
                .unwrap()
                .messages
                .len()
        }
    };
    assert_eq!(find("zebra").await, 1);
    assert_eq!(find("giraffe").await, 0);
    let asked = fake.server().searches.len();
    assert_eq!(find("from:priya@example.com").await, 1);
    assert_eq!(
        fake.server().searches.len(),
        asked,
        "an operator needs no server"
    );
}

#[allow(dead_code)]
fn _types(_: &Served<FakeStore>) {}
