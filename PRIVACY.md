# Kinlink Privacy Policy

*Last updated: November 21, 2026*

Kinlink is made by Abdulkareem Alsaou ("the author"). Short version: **the author collects
nothing about you.** There are no accounts, no analytics, no ads and no Kinlink servers.

## What stays on your computer

Stored only on your own device, in your user folder:

- your username, a secret key (encrypted with Windows DPAPI on PCs) and the ID made from it;
- your friends list, nicknames, groups and the permissions you gave each friend;
- your settings and a log of what the app did (Settings → Open log), which can include IP
  addresses of connections;
- screenshots, recordings and received files, in the folders you can see in the app.

The author never receives any of this. You can delete it by uninstalling Kinlink and removing
the `ParsecCloneNative` folder in `%APPDATA%`.

## What goes to other people

- **Friends you connect with** receive what you choose to share in a session: your screen, sound,
  voice (when the mic is on), chat, files, clipboard and controller input. This travels
  **directly** between the two computers, encrypted end to end. Like any direct connection, the
  other side can see your IP address.
- **Friend requests and connection requests** go through public MQTT message servers run by
  third parties (broker.emqx.io, broker.hivemq.com). The messages are end-to-end encrypted and
  sent under scrambled topic names, so those servers can't read them or tell who is talking to
  whom, but they do see your IP address and when you're online, like any internet service.

## Other services the app contacts

- **STUN servers** run by Google and Cloudflare, to learn your public address for direct
  connections.
- **Your own router** (only if "Open router port (UPnP)" is on), to open a port for friends.
- **GitHub**, to check for and download updates from this repository's public releases.
- **Cloudflare**, only when you run the optional network test.

Each of these sees your IP address when contacted and follows its own privacy policy.

## Children

Kinlink is not meant for children under 13 without a parent's supervision.

## Your choices

Don't accept connections from people you don't trust, turn off "Let friends connect" when you
don't want to be reachable, and use the panic button (Ctrl+Alt+Shift+F12) to disconnect everyone.

## Changes and contact

Changes to this policy are published in this repository. Questions:
https://github.com/IamAbood/Kinlink/issues
