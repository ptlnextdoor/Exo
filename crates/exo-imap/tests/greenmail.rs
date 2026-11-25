//! The real IMAP client against GreenMail, for what the fake server cannot show: that
//! `async-imap`'s commands and parsing match what a server sends.
//!
//! Start GreenMail first, then run with `--ignored`:
//!
//! ```text
//! java -Dgreenmail.setup.test.imap -Dgreenmail.hostname=127.0.0.1 -Dgreenmail.auth.disabled \
//!     -jar greenmail-standalone.jar
//! cargo test -p exo-imap --test greenmail -- --ignored
//! ```
//!
//! `EXO_TEST_IMAP` names another server (`host:port`, plain IMAP). Each run logs in as a login
//! of its own, so it starts from an empty mailbox. GreenMail has no CONDSTORE, which makes this
//! the check of the fallback path. `EXO_TEST_SMTP` (default `127.0.0.1:3025`) is its outgoing
//! server, started with `-Dgreenmail.setup.test.smtp`.

mod support;

use std::sync::Arc;

use async_imap::Client;
use exo_imap::{Facade, Mailstore};
use futures::TryStreamExt;
use gmail::{Format, ModifyMessageRequest};
use support::{ACCOUNT, mail, messages, serve};
use tokio::net::TcpStream;

fn address() -> String {
    std::env::var("EXO_TEST_IMAP").unwrap_or_else(|_| "127.0.0.1:3143".into())
}

async fn session(login: &str) -> async_imap::Session<TcpStream> {
    let mut client = Client::new(TcpStream::connect(address()).await.unwrap());
    client.read_response().await.unwrap().unwrap();
    client
        .login(login, "any")
        .await
        .map_err(|(error, _)| error)
        .unwrap()
}

#[tokio::test]
#[ignore = "needs a GreenMail server"]
async fn reads_a_real_server() {
    let login = format!("kahlus{}", std::process::id());
    let mut other = session(&login).await;
    other.create("Clients").await.unwrap();
    let date = Some("\"17-Nov-2026 09:00:00 +0000\"");
    other
        .append(
            "INBOX",
            None,
            date,
            mail(
                "Priya <priya@example.com>",
                "Launch",
                "a@x",
                None,
                "Ship it?",
            ),
        )
        .await
        .unwrap();
    other
        .append(
            "Clients",
            Some("(\\Seen \\Flagged)"),
            date,
            mail("Bo <bo@example.com>", "Invoice", "c@x", None, "Attached."),
        )
        .await
        .unwrap();

    let stream = TcpStream::connect(address()).await.unwrap();
    let mut store = exo_imap::imap::login(stream, &login, "any").await.unwrap();
    let listed = store.folders().await.unwrap();
    let names: Vec<_> = listed.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.contains(&"INBOX") && names.contains(&"Clients"),
        "{names:?}"
    );

    let facade = Arc::new(Facade::open(store, None, ACCOUNT, "Aayushya").unwrap());
    let served = serve(facade.clone()).await;
    assert_eq!(facade.sync().await.unwrap().added, 2);
    let all = messages(&served.gmail).await;
    assert!(all["Launch"].2.contains(&"UNREAD".to_owned()));
    assert!(all["Launch"].2.contains(&"INBOX".to_owned()));
    assert!(all["Invoice"].2.contains(&"STARRED".to_owned()));
    assert!(!all["Invoice"].2.contains(&"UNREAD".to_owned()));

    // A flag set by another client.
    other.select("INBOX").await.unwrap();
    let _: Vec<_> = other
        .uid_store("1", "+FLAGS (\\Seen)")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(facade.sync().await.unwrap().flagged, 1);
    assert!(
        !messages(&served.gmail).await["Launch"]
            .2
            .contains(&"UNREAD".to_owned())
    );

    // Its text, fetched when asked for.
    let message = served
        .gmail
        .gmail()
        .message(&all["Launch"].0, Format::Full, &[])
        .await
        .unwrap();
    let text = message.payload.unwrap().body.unwrap().data.unwrap();
    assert_eq!(String::from_utf8(text).unwrap().trim(), "Ship it?");
}

fn smtp() -> exo_imap::imap::Smtp {
    let address = std::env::var("EXO_TEST_SMTP").unwrap_or_else(|_| "127.0.0.1:3025".into());
    let (host, port) = address.split_once(':').unwrap();
    exo_imap::imap::Smtp::builder_dangerous(host)
        .port(port.parse().unwrap())
        .build()
}

async fn uids(session: &mut async_imap::Session<TcpStream>, folder: &str) -> Vec<u32> {
    session.select(folder).await.unwrap();
    let mut uids: Vec<u32> = session
        .uid_search("ALL")
        .await
        .unwrap()
        .into_iter()
        .collect();
    uids.sort_unstable();
    uids
}

#[tokio::test]
#[ignore = "needs a GreenMail server"]
async fn changes_and_sends_reach_a_real_server() {
    let login = format!("writer{}", std::process::id());
    let mut other = session(&login).await;
    other
        .append(
            "INBOX",
            None,
            Some("\"17-Nov-2026 09:00:00 +0000\""),
            mail(
                "Priya <priya@example.com>",
                "Launch",
                "a@x",
                None,
                "A zebra.",
            ),
        )
        .await
        .unwrap();

    let stream = TcpStream::connect(address()).await.unwrap();
    let store = exo_imap::imap::login(stream, &login, "any")
        .await
        .unwrap()
        .with_smtp(smtp());
    let facade = Arc::new(Facade::open(store, None, ACCOUNT, "Aayushya").unwrap());
    let served = serve(facade.clone()).await;
    facade.sync().await.unwrap();
    let id = messages(&served.gmail).await["Launch"].0.clone();
    let gmail = served.gmail.gmail();

    // Read and starred, then archived: flags, and a move to a folder the server did not have.
    gmail
        .modify_message(
            &id,
            &ModifyMessageRequest {
                add_label_ids: vec!["STARRED".into()],
                remove_label_ids: vec!["UNREAD".into(), "INBOX".into()],
                ..ModifyMessageRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(uids(&mut other, "INBOX").await.is_empty());
    let archived = uids(&mut other, "Archive").await;
    assert_eq!(archived.len(), 1);
    let flags: Vec<String> = other
        .uid_fetch(archived[0].to_string(), "FLAGS")
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap()[0]
        .flags()
        .map(|f| format!("{f:?}"))
        .collect();
    assert!(
        flags.iter().any(|f| f.contains("Seen")) && flags.iter().any(|f| f.contains("Flagged")),
        "{flags:?}"
    );
    assert_eq!(facade.sync().await.unwrap(), Default::default());

    // Server text search finds what the headers do not say.
    let found = gmail
        .messages(&gmail::ListQuery {
            q: Some("zebra".into()),
            ..gmail::ListQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(found.messages.len(), 1);

    // Sending hands the message to SMTP and files the copy in Sent, once.
    let raw = format!(
        "From: {ACCOUNT}\r\nTo: {login}@example.com\r\nBcc: hidden@example.com\r\n\
         Subject: Out\r\nMessage-ID: <out@x>\r\nDate: Tue, 17 Nov 2026 10:00:00 +0000\r\n\r\nHi.\r\n"
    );
    gmail.send(raw.into_bytes(), None).await.unwrap();
    let sent = uids(&mut other, "Sent").await;
    assert_eq!(sent.len(), 1);
    assert_eq!(facade.sync().await.unwrap(), Default::default());
    assert_eq!(uids(&mut other, "Sent").await.len(), 1);

    // Delete for good.
    gmail.trash_message(&id).await.unwrap();
    assert_eq!(uids(&mut other, "Trash").await.len(), 1);
    gmail.delete_message(&id).await.unwrap();
    assert!(uids(&mut other, "Trash").await.is_empty());
    assert_eq!(facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
#[ignore = "needs a GreenMail server"]
async fn moves_by_copy_when_the_server_cannot_move() {
    let login = format!("copier{}", std::process::id());
    let mut other = session(&login).await;
    other
        .append(
            "INBOX",
            None,
            None,
            mail("Bo <bo@example.com>", "Old", "a@x", None, "Hi"),
        )
        .await
        .unwrap();
    let stream = TcpStream::connect(address()).await.unwrap();
    let store = exo_imap::imap::login(stream, &login, "any")
        .await
        .unwrap()
        .without_move();
    let facade = Arc::new(Facade::open(store, None, ACCOUNT, "Aayushya").unwrap());
    let served = serve(facade.clone()).await;
    facade.sync().await.unwrap();
    let id = messages(&served.gmail).await["Old"].0.clone();
    served
        .gmail
        .gmail()
        .modify_message(
            &id,
            &ModifyMessageRequest {
                remove_label_ids: vec!["INBOX".into()],
                ..ModifyMessageRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(uids(&mut other, "INBOX").await.is_empty());
    assert_eq!(uids(&mut other, "Archive").await.len(), 1);
    assert_eq!(facade.sync().await.unwrap(), Default::default());
}

#[tokio::test]
#[ignore = "needs a GreenMail server"]
async fn idle_returns_when_mail_arrives() {
    let login = format!("idler{}", std::process::id());
    let mut other = session(&login).await;
    let stream = TcpStream::connect(address()).await.unwrap();
    let store = exo_imap::imap::login(stream, &login, "any").await.unwrap();
    let waiting = tokio::spawn(store.idle(std::time::Duration::from_secs(30)));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    other
        .append(
            "INBOX",
            None,
            None,
            mail("Bo <bo@example.com>", "New", "n@x", None, "Hi"),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .expect("woke up on new mail")
        .unwrap()
        .unwrap();
}
