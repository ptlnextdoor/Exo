//! Exo's own store, unchanged, syncing from the facade.

mod support;

use std::time::Duration;

use exo_store::{ListId, Mutation, Options, Store};
use gmail::{Endpoints, StaticToken};
use support::{ACCOUNT, Fake, TOKEN, mail, open, serve};

const T0: i64 = 1_763_370_000_000;

async fn open_store(dir: &std::path::Path, origin: &str) -> Store {
    let options = Options {
        quota: None,
        backoff: Duration::from_millis(5),
        ..Options::default()
    };
    Store::open_with(
        dir,
        ACCOUNT,
        Endpoints::mock(&origin.parse().unwrap()),
        StaticToken(TOKEN.into()),
        options,
    )
    .await
    .unwrap()
}

async fn sync_all(store: &Store) -> exo_store::SyncReport {
    for _ in 0..50 {
        let report = store.sync().await.unwrap();
        if !report.backfilling {
            return report;
        }
    }
    panic!("backfill never finished");
}

#[tokio::test]
async fn the_store_syncs_reads_and_survives_a_restart() {
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
        &["\\Seen"],
        T0 - 86_400_000,
    );
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("facade.sqlite3");
    let facade = open(&fake, Some(&index));
    let served = serve(facade.clone()).await;

    // First run: the store learns the mailbox from nothing but the facade.
    let store = open_store(dir.path(), &served.origin).await;
    sync_all(&store).await;
    let inbox = store.list(&ListId::Inbox, None, 10).await.unwrap().threads;
    assert_eq!(inbox.len(), 1, "one conversation, with its reply from Sent");
    assert_eq!(inbox[0].subject, "Launch");
    assert!(inbox[0].unread);
    assert_eq!(inbox[0].participants[0].email, "priya@example.com");

    // Read elsewhere, and a new message. The facade asks the server once it is two seconds stale.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    fake.set_flags("INBOX", 1, &["\\Seen"]);
    fake.deliver(
        "INBOX",
        mail("New <new@example.com>", "Hello", "d@x", None, "Hi"),
        &[],
        T0 + 120_000,
    );
    let report = sync_all(&store).await;
    assert!(!report.rebuilt);
    let inbox = store.list(&ListId::Inbox, None, 10).await.unwrap().threads;
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox[0].subject, "Hello");
    assert!(inbox[0].unread);
    assert_eq!(inbox[1].subject, "Launch");
    assert!(!inbox[1].unread, "read in another client");

    // Close everything and start again. The store's cursor still means something to the facade.
    drop(store);
    drop(served);
    drop(facade);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    fake.deliver(
        "INBOX",
        mail("Late <late@example.com>", "Later", "e@x", None, "Hi"),
        &[],
        T0 + 180_000,
    );
    let served = serve(open(&fake, Some(&index))).await;
    let store = open_store(dir.path(), &served.origin).await;
    let report = sync_all(&store).await;
    assert!(
        !report.rebuilt,
        "a restart must not make Exo rebuild its cache"
    );
    assert_eq!(report.arrived.len(), 1);
    let inbox = store.list(&ListId::Inbox, None, 10).await.unwrap().threads;
    assert_eq!(inbox.len(), 3);
    assert_eq!(inbox[0].subject, "Later");
}

#[tokio::test]
async fn the_stores_actions_reach_the_server() {
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
    let dir = tempfile::tempdir().unwrap();
    let served = serve(open(&fake, None)).await;
    let store = open_store(dir.path(), &served.origin).await;
    sync_all(&store).await;
    let inbox = store.list(&ListId::Inbox, None, 10).await.unwrap().threads;
    let thread = inbox[0].id.clone();

    // Starred, read and done, as the keys `s`, `u` and `e` do.
    for mutation in [Mutation::Star, Mutation::MarkRead, Mutation::MarkDone] {
        store
            .apply(std::slice::from_ref(&thread), mutation)
            .await
            .unwrap();
    }
    let report = store.flush().await.unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    {
        let server = fake.server();
        assert!(server.folders["INBOX"].messages.is_empty());
        let archived: Vec<_> = server.folders["Archive"].messages.values().collect();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].flags.contains(&"\\Flagged".to_owned()));
        assert!(archived[0].flags.contains(&"\\Seen".to_owned()));
    }

    // What the server now holds is what the store holds: a sync changes nothing.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let report = sync_all(&store).await;
    assert!(!report.rebuilt);
    assert!(
        store
            .list(&ListId::Inbox, None, 10)
            .await
            .unwrap()
            .threads
            .is_empty()
    );
}
