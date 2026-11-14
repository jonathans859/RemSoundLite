# RemSoundLite

A small, fast, accessible Windows client for the RemSound audio protocol. It does two things:
it sends a sound card's audio to your peers, and it plays what they send you.

## Status

**Alpha/beta.** The project is only a few days old, but it already works well for everyday use.
It's public so more people can try it and find what breaks. Please open an issue when you do.

The lock-screen service (see below) is **untested** so far.

## Download

- **Development build** (rebuilt from every push to `main`):
  [RemSoundLite.zip](https://github.com/jonathans859/RemSoundLite/releases/download/latest/RemSoundLite.zip)
  ([release page](https://github.com/jonathans859/RemSoundLite/releases/tag/latest))
- No stable release yet. Once there is one, it will appear under
  [Releases](https://github.com/jonathans859/RemSoundLite/releases).

It's a single `remsoundlite.exe` with nothing to install. To keep getting development builds
through the built-in updater, choose **Development builds** in Settings (Ctrl+,), Updates.

## Features

- **Send** from any input device, or from any output device in loopback (whatever that device is
  playing). Opus (live or broadcast quality) or uncompressed 24-bit PCM.
- **Receive** with a jitter buffer that can tune itself as conditions change.
- **Peers** found on the local network or added by IP address, protected by a shared password.
- **Profiles**: save every setting under a name and switch between them (Ctrl+S, Ctrl+Shift+S,
  Ctrl+P). Launch opens the last one used, or one you pick in Settings, General.
- **Notification area**: hide the window (Ctrl+H), optionally start hidden.
- **Start after logging in to Windows**, if you turn it on in Settings, General. With "Start
  hidden" as well, only the notification-area icon appears.
- **Global keyboard shortcuts**, which work in any program: turn sending or receiving on, off or
  toggle it, show the window, or switch to a profile. You set them in Settings, Keyboard
  shortcuts; none are set by default.
- **Lock-screen service** (untested): an optional Windows service that starts at boot and sends
  audio before anyone signs in, for example a screen reader on the sign-in screen. It steps aside
  while the window is open.
- **Updates** from a Stable or Development channel, with signed downloads. By default RemSoundLite
  checks at startup and offers a new version as soon as one is out (Settings, Updates).

## Building

Requires Rust 1.88+, the MSVC build tools, CMake, and LLVM (for `libclang`; set
`LIBCLANG_PATH` if it isn't found).

```
cargo build --release
```

## License

MIT, see [LICENSE](LICENSE).

The download also contains `THIRD_PARTY_NOTICES.txt`, the licences of the libraries built into
the exe. To make it yourself after a release build, install `cargo-about`
(`cargo install cargo-about --locked --features cli`) and run
`python tools/third_party_notices.py`.

The shortcut capture dialog (`src/gui/capture.rs`) is adapted from [wx-utils](https://github.com/accessware/wx-utils), MIT License, Copyright (c) 2026 Quin Gillespie. Its full notice is at the top of that file.
