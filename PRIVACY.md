# Kinlink Privacy Policy

*Last updated: November 23, 2026*

Kinlink is made by Abdulkareem Alsaou ("the author"). Short version: **the author collects
nothing about you.** There are no accounts, no analytics, no ads and no Kinlink servers.

The author does not sell, rent or share personal data, because the author never receives any.

## What stays on your computer

Stored only on your own device, in your user folder:

- your username, a secret key (encrypted with Windows DPAPI on PCs) and the ID made from it;
- your friends list, nicknames, groups, group chats, chat history, profile picture choice and
  the permissions you gave each friend;
- your settings and a log of what the app did (Settings → Open log), which can include IP
  addresses of connections;
- screenshots, recordings and received files, in the folders you can see in the app.

The author never receives any of this. You can delete it by uninstalling Kinlink and removing
the `ParsecCloneNative` folder in `%APPDATA%` (on the phone: uninstall the app or clear its data).

## What goes to other people

- **Friends you connect with** receive what you choose to share in a session: your screen, sound,
  voice (when the mic is on), chat, files, clipboard and controller input. This travels
  **directly** between the two computers, encrypted end to end. Like any direct connection, the
  other side can see your IP address.
- **Your username, ID and profile picture** are shown to your friends and to the members of
  group chats you are in.
- **Friend requests, connection requests, chat and group chat messages, and mini-game moves**
  go through public MQTT message servers run by
  third parties (broker.emqx.io, broker.hivemq.com). The messages are end-to-end encrypted and
  sent under scrambled topic names, so those servers can't read them or tell who is talking to
  whom, but they do see your IP address and when you're online, like any internet service.
- **People on your local network** (only while you are hosting) can see your Kinlink ID, which
  the app announces so nearby PCs can add you as a friend.
- **Browser and watch-party guests** load the browser client from GitHub Pages and then connect
  directly to you; they see what you share, like any other guest.

Anything you send to someone is then on their device. The author can't delete or recall it.

## Other services the app contacts

- **STUN servers** run by Google and Cloudflare, to learn your public address for direct
  connections.
- **Your own router** (only if "Open router port (UPnP)" is on), to open a port for friends.
- **GitHub**, to check for and download updates from this repository's public releases.
- **Cloudflare**, only when you run the optional network test.

Each of these sees your IP address when contacted and follows its own privacy policy.

## Children

Kinlink is not meant for children under 13. Children under 13 must not use it, and older
minors should use it with a parent's permission. The author does not knowingly collect any
data from anyone, including children.

## Your rights

Because the author holds no data about you, there is nothing for the author to access, correct,
export or delete on request; you control all of it on your own device. Laws like the GDPR (EU/UK)
or CCPA (California) still give you rights against the third-party services listed above; contact
them directly. The author is not a "controller" or "processor" of your data and does not run
any Kinlink server.

## Security

Connections and messages are end-to-end encrypted, but no software is perfectly secure. Keep
your computer updated, protect your device and only accept people you trust.

## Your choices

Don't accept connections from people you don't trust, turn off "Let friends connect" when you
don't want to be reachable, and use the panic button (Ctrl+Alt+Shift+F12) to disconnect everyone.

## Changes and contact

Changes to this policy are published in this repository. Questions:
https://github.com/IamAbood/Kinlink/issues
