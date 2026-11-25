# Run Exo on Kahlus mail (Purelymail)

This page says what the Kahlus mailbox owner must give, and what to do on the Mac, to run Exo's IMAP facade (`exo-imap`) against a Purelymail mailbox.

## Status

| Part | State |
|---|---|
| `exo-imap`: reads, flags, moves, delete, send, search, IDLE | Built. Tested on Linux against a fake server and against GreenMail. |
| Real Purelymail mailbox | Not tried. No credentials were available. |
| Exo.app (macOS) using the facade | **Not done.** Exo has no "IMAP account" type yet: it signs in to Google only. |

You can run the facade and read your mailbox through its Gmail-style HTTP API today. You cannot yet see the mail inside Exo.app.

## What you give

1. **A Purelymail app password.** Do not give the main password. Never put either password in a file, a chat or the repository.
   - Make the app password in your Purelymail account settings. If the page offers scopes, allow IMAP and SMTP only. (The exact page name is not verified.)
   - Delete it in Purelymail when you stop using Exo. This cuts off access at once.
2. **Two facts about the Mac.** Run these two commands and send the output:
   - `df -h /` (free disk space).
   - `xcodebuild -version` (Xcode version).
3. **A go-ahead for step 5** (the probe). It reads folder names and message counts. It prints no message text.

## What the Mac needs

| Need | For | Amount |
|---|---|---|
| Rust 1.98 or later | The facade and the app | Install with `rustup`. |
| About 25 to 30 GB free disk | The full Exo.app build (plan figure) | Free it first. The facade alone needs much less (not measured). |
| Xcode 27 | The Exo.app build only (see the README) | Not needed for the facade. |
| Ruby 4.0 and Bundler | The Exo.app build only | Not needed for the facade. |

## Steps

Replace `you@kahlus.com` with the full mailbox address.

1. **Get the code.**

   ```sh
   git clone https://github.com/ptlnextdoor/Exo.git && cd Exo
   git checkout fm/exo-purelymail-build
   ```

2. **Put the app password in the Keychain.** The command asks for the password. It does not show it.

   ```sh
   security add-generic-password -s "Exo IMAP password" -a you@kahlus.com -w
   ```

3. **Build the facade.**

   ```sh
   cargo build --release -p exo-imap
   ```

4. **Make a place for its index.** The index keeps message IDs the same after a restart.

   ```sh
   mkdir -p ~/.exo-imap
   ```

5. **Probe the server.** This is the check that finishes the facts the plan could not verify.

   ```sh
   EXO_IMAP_PASSWORD="$(security find-generic-password -s 'Exo IMAP password' -a you@kahlus.com -w)" \
     target/release/exo-imap --probe --user you@kahlus.com --index ~/.exo-imap/kahlus.sqlite3
   ```

   Send the output back. It shows the capabilities, each folder with its attributes and message count, and `permanent=` flags. If `permanent=` has no `\*`, mute and snooze fail (the hidden labels are IMAP keywords).

6. **Run the facade.**

   ```sh
   EXO_IMAP_PASSWORD="$(security find-generic-password -s 'Exo IMAP password' -a you@kahlus.com -w)" \
     target/release/exo-imap --user you@kahlus.com --name "Your Name" --index ~/.exo-imap/kahlus.sqlite3
   ```

   The first start reads the headers of every folder, which can take minutes for a large mailbox. It then prints one line: `http://127.0.0.1:PORT TOKEN`. It listens on loopback only.

7. **Read the mailbox.** In a second terminal:

   ```sh
   curl -H "Authorization: Bearer TOKEN" http://127.0.0.1:PORT/gmail/v1/users/me/profile
   curl -H "Authorization: Bearer TOKEN" "http://127.0.0.1:PORT/gmail/v1/users/me/threads?maxResults=5&labelIds=INBOX"
   ```

   Mark one thread read in Kahlus webmail. The change shows in the next `threads` or `history` request.

## Do not do yet

- **Do not send mail to test.** A send is real mail from your address. When you decide to test it, send to your own Gmail address first, and tell me before you do. Check that Sent in Purelymail holds exactly one copy. The facade files its own copy unless Sent already has one. If Sent shows two copies, tell me.
- **Do not run the facade on a shared machine.** Any program that has the token can read and change your mailbox.

## What is still open

These are the unknowns. The probe and one real send answer the first three.

1. Whether Purelymail accepts custom keywords (`PERMANENTFLAGS`).
2. Whether Purelymail saves sent mail to Sent by itself.
3. The real folder names and the size of the mailbox.
4. Exo.app wiring: an IMAP account type, the Keychain service `Exo IMAP password`, and no Google-only features (calendar, contacts, photos, brand logos) for that account. This is the next milestone and needs the Mac build.
5. Drafts are kept in memory only. A restart of the facade loses them.
