# Kinlink

Made by **Abdulkareem Alsaou**.

Low-latency remote desktop for Windows, written in Rust. Works like Parsec: add friends, see who's online, click Connect.
No accounts, no logins, no server of our own.

## Use

Run `ParsecClone-<version>.exe` (~19 MB) on both PCs.

1. **First launch:** pick a username. You get an ID like `abood#4821`. The number comes from a secret key created on your PC, so nobody can pretend to be you.
2. **Add a friend:** type their `name#1234` and click **Add friend**. They click **Accept**.
3. **Connect:** when a friend is online (green dot), click **Connect**. They get a popup to **Accept / Decline**.
4. **Several friends can join the same PC at once** (up to 8), e.g. couch co-op: each guest's controller becomes its own virtual pad.
5. **While connected:** you control the PC fully (mouse, keyboard including Alt+Tab / Win / Esc, gamepads) and hear its audio.
   A small floating box (top-left) has **Disconnect, Resolution, Quality, Volume/Mute, Monitor** and live stats.

**Per-friend permissions:** click **Edit** next to a friend: *Allow mouse*, *Allow keyboard*, *Connect without asking me*.

**Hotkeys while connected:** `Ctrl+Alt+Shift` + `Q` disconnect · `O` hide/show the menu · `R` lock mouse (games) · `F` fullscreen · `M` next monitor.

**Immersive by default:** sessions open fullscreen (Settings to turn off), the menu button only appears when your mouse reaches the top-left corner, and the remote cursor is drawn at your own mouse position so it never lags.

**Panic button (host):** `Ctrl+Alt+Shift+F12` anywhere, the tray menu, or *Disconnect everyone* kicks every guest and pauses sharing.

**Files:** drag files onto the session window to send them to the host; on the host, drop files on the app window to send them to every guest. Received files go to *DownloadsKinlink*. Transfers survive packet loss and show progress with Cancel.
**Chat:** text chat in sessions (menu → Chat, or `Ctrl+Alt+Shift+C`); while the chat box is open your keys stay local. The host has a chat box under *This PC*.
**Voice:** menu → *Mic* or `Ctrl+Alt+Shift+V` (host: *Talk to guests*). Shows who is talking. The host's stream excludes the app's own audio, so voices aren't echoed back (Windows 10 2004+). Use headphones.
**Screenshots / recording:** menu → *Screenshot* (`Ctrl+Alt+Shift+P`, PNG in *PicturesKinlink*) and *Record* (MP4 with audio in *VideosKinlink*; video is saved as received, no re-encoding).

**Lock screen & admin prompts:** Settings → *Enable (asks for admin)* installs a small Windows service. While friends are connected, it lets them see and use the lock screen, UAC prompts and admin apps (a SYSTEM helper captures and types on the secure desktop). It only answers the genuine Kinlink exe, and survives updates without new admin prompts. *Turn off* removes it.
**PlayStation rumble:** game vibration now reaches DualShock 4 / DualSense controllers too (USB and Bluetooth).

**Friends list:** click **Edit** on a friend to give them a **nickname** and a **group** (e.g. "Home PCs"); groups show how many are online.
**Desktop shortcut:** Edit → *Desktop shortcut* makes a "Connect to …" icon that connects in one click (also while the app is already running).
**Notifications:** Windows notifies you when a friend comes online or sends a friend request (Settings to turn off; Windows Focus Assist can hide the pop-up).
**Network test:** Settings → *Run network test* checks the friend network, router type, UPnP, IPv6, Windows Firewall, Vanguard and the controller driver, measures speed (Cloudflare speed test) and suggests a max bitrate.

**Background:** starts with Windows, lives in the tray (by the clock). Closing the window keeps it running; Quit from the tray icon.

**Gamepads:** Xbox and PlayStation (DualShock 4 / DualSense) controllers work. Each guest's controller becomes its own virtual pad on the host (PlayStation pads appear as a DualShock 4, so games show PS prompts). Game vibration is sent back to Xbox-style controllers. The host can reorder **player slots** (Up/Down under Controllers). The PC being controlled needs the [ViGEmBus driver](https://github.com/nefarius/ViGEmBus/releases).

**Spectators:** untick *Allow mouse*, *Allow keyboard* and *Allow controller* for a friend and they can only watch. Changes apply instantly, even mid-session.

## How it works / privacy
- Friends find each other through two free public MQTT brokers (used at the same time for redundancy).
  Every message is end-to-end encrypted (X25519 + XChaCha20-Poly1305) and topics are hashed:
  the brokers relay only random-looking bytes and can't tell who is who.
- The session itself goes **directly PC-to-PC** (UDP hole punching via STUN, optional UPnP) and is encrypted
  (ChaCha20-Poly1305, per-session keys, replay protection). Encryption costs ~0.01 ms per frame.
- Your secret key is stored with Windows DPAPI (tied to your Windows account).
- Limitation: if **both** PCs are on very strict networks (some mobile/corporate), a direct path can't be opened and there is no relay.

## Features
DXGI capture with cursor · NVIDIA/AMD/Intel hardware H.264 (software fallback) · custom UDP protocol with keyframe-on-loss
and loss-based bitrate · wgpu renderer · system audio · clipboard text sync · multi-monitor · relative mouse · gamepads.
Measured on one PC (RTX 2060 Super): ~6–10 ms capture → decoded.

## Android app (phone controls PC)

Install `Kinlink.apk` on the phone (Android 8.0 or newer, 64-bit). You may need to allow "install unknown apps" for your file manager or browser. Pick a username, add your PC's name#tag as a friend, and accept the request on the PC. Then tap **Connect**.

- One finger moves the mouse; tap to left-click.
- Two-finger tap right-clicks; two fingers scroll.
- Hold, then drag, to drag.
- ☰ opens the menu: phone keyboard, Esc/Tab/Enter/Win/Alt+Tab/Ctrl+C/V, arrow keys, disconnect.
- Streams at 720p / 10 Mbps.
- Not on the phone yet: audio, voice, files, clipboard, controllers, and hosting.

Build: `sh scripts/build-android.sh` (needs Android SDK + NDK 26, JDK 17, `cargo install cargo-apk`, `rustup target add aarch64-linux-android`). Writes `dist/Kinlink.apk`.

## Build & test (in `native/`)
Needs Rust (MSVC) and Visual Studio C++ Build Tools.
```
cargo build --release
cargo test --release                                   # crypto, STUN, codecs (incl. GPU encoder), scaling, audio
set PCLONE_DRY_INPUT=1 && target\release\pclone.exe --selftest     # encrypted host+client loopback
target\release\pclone.exe --signaltest                 # friend request / presence / connect via the real brokers
```
Options: `--minimized`, `--connect-friend name#1234`, `--profile <name>` (separate test identity).

## Older versions
`ui/` + `src-tauri/`: v0.2 WebRTC/PeerJS app, kept for reference.

## License

MIT, see [LICENSE](LICENSE). How Windows builds are made and signed: [CODE_SIGNING.md](CODE_SIGNING.md).
