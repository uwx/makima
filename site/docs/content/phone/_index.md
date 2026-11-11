+++
title = "Phone"
weight = 25
[extra]
group = "Guides"
+++

# Phone

The phone app is a second window on a session your computer runs. It draws the
same chat panel the desktop window draws, so the agent, the model and the files
stay where they are, and the phone shows the conversation and drives turns.

## Share a session

Sharing needs a listening agent and a token the other machine presents:

```bash
makima --share 0.0.0.0:7331 --share-token my-token
```

`--share` binds every interface on 7331, so the phone reaches it at the
computer's address on the same network, for example `lan://192.168.2.4:7331`.
Without `--share-token` the agent uses the token in its state directory, which
the phone never sees, so pass one you can type. The desktop window's status bar
does the same with `Share`, and shows both the link and the token to copy.

The listener speaks plain TCP, and the token is its only gate. Use it on a
network you trust, or over a private one such as Tailscale.

## Build and install

The app is one APK with no Java and no Gradle. The build uses the Android SDK's
own `aapt2`, `zipalign` and `apksigner`, and cross compiles the Rust library
with the NDK's linker:

```bash
just android-apk                              # arm64 and x86_64
just android-apk --abi arm64-v8a              # one of them
just android-run                              # debug build, install, start
```

Both need `ANDROID_HOME` or `ANDROID_SDK_ROOT`, the SDK packages `platform-tools`
and `build-tools`, an NDK, and the `aarch64-linux-android` Rust target. On an
emulator use `-gpu host`; `swiftshader_indirect` starves the guest and `adb`
stops answering.

## Connect

The first screen asks for the link and the token. It writes both to
`attach.json` in the app's data directory, and every later start reconnects
without asking. A connection that fails reopens the form with what was typed,
so a typo costs one field, not the whole link.

```
+--------------------------+
| makima                   |
| A coding agent, in your  |
| pocket                   |
|                          |
| [ lan://192.168.2.4:... ]|
| [ the token             ]|
| [ Connect               ]|
+--------------------------+
```

## What it does

The window replays the session and then follows it live: streamed text,
thinking, tool calls and token counts, the same as the machine that runs it. A
turn another window starts appears here too, under a line saying where it runs.

The composer sends a prompt to the machine that owns the session. Prompting
takes control of the session when it is idle, so two windows never type into
the same turn, and a busy session answers with the other window's turn still
running.

Typing opens the system keyboard. A plain `NativeActivity` has no input
connection to the keyboard, so the app asks for the keyboard itself, reads the
keys back as key events and measures the bottom inset a moment later to know
how much of the window the keyboard took.

## Not there yet

- No session picker: the phone connects to the one link it saved.
- If the session ends while the phone watches, the window says so and offers the
  form again; it does not search for the session elsewhere.
