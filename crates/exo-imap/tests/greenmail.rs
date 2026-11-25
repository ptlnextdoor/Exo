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
//! the check of the fallback path.

mod support;

use std::sync::Arc;

use async_imap::Client;
use exo_imap::{Facade, Mailstore};
use futures::TryStreamExt;
use gmail::Format;
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
