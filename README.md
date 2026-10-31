# Whisker

A Matrix client written in Rust and built for privacy, running on Linux, Windows and
Android. It keeps every account on every server signed in at once, can hide alt accounts
behind a second passphrase, and can run an AI as its own automated account with moderation
rights that you control.

Whisker is part of the anti-ghostsec family: the **anoncat** homeserver, the **FerroOS**
operating system and the **Wisp** browser.

## Features

- **Sign in or create accounts.** Pick a suggested server (anoncat.gleeze.com) and type
  just your username, or choose *Other server…* and enter any server; a full address like
  `@you:matrix.org` works too. Sign-up works on servers that allow open registration or
  hand out registration tokens; servers that need an email or captcha say so.
- **Many accounts, signed in at once.** Switch between any account on any server with
  one click. Each account syncs in the background, so nothing goes stale.
- **Tags.** Label accounts (`main`, `alt`, `work`, …) and filter the account list by tag.
  Tags never leave your device.
- **Hidden vaults.** All accounts are stored in an encrypted vault (Argon2id 64 MiB/3 passes
  + XChaCha20-Poly1305, the same setup as Enclave). You can make more than one vault, each
  with its own passphrase. Your main passphrase opens only the main vault. Alts in a hidden
  vault don't appear and aren't mentioned anywhere in the main vault until you unlock it
  with its own passphrase. **Lock** closes all vaults. See [Hidden vaults on disk](#hidden-vaults-on-disk)
  for what someone with your files can and can't tell.
- **A proxy per account.** You can give each account its own proxy (for example a Tor SOCKS
  port), so a server can't link your alts by IP address.
- **AI accounts.** Each AI signs in as its own Matrix account, separate from yours, with its
  own settings page. Paste the address of any AI API (OpenRouter, Groq, Mistral, DeepSeek,
  Gemini, OpenAI, LM Studio, llama.cpp, vLLM, Ollama, or any Anthropic-style API). Whisker
  works out what kind of API it is, lists its models, and has a Test button.
- **Admin controls.** In the composer, type `/kick`, `/ban`, `/unban`, `/invite`, `/redact`,
  `/op @user 50` or `/deop`. The AI uses the same actions.
- **Server plugins.** If the homeserver runs hot-loaded WASM plugins (anoncat), Whisker
  can find them at runtime and call their methods (no UI uses them yet). See
  [docs/PLUGIN_PROTOCOL.md](docs/PLUGIN_PROTOCOL.md).
- **Chat:** Markdown formatting (bold, italic, strikethrough, `code`, code blocks, quotes,
  lists, headings, clickable links); replies, edits and deletions; image previews with a
  full-size viewer; file cards; date dividers; a multi-line composer (Enter sends,
  Shift+Enter adds a line); and an **attach** button for images and files (up to 100 MB).
- **Drag to select** any part of a message and copy it with Ctrl+C (turn on *Rich message
  text* in Settings for bold/italic/clickable links instead; rich text can't be selected).
  Click a **name** to reply to that message; click an **avatar** to open the person's
  profile (direct chat, mention, copy id, kick/ban if you're a moderator, ignore).
- **Drag an account** up or down the rail to reorder (or right-click it for Move up/down). **Right-click a room** to
  pin it to the top (saved as a favourite, so other apps see it), mark it read, copy its
  address or leave it. Opening a room clears its unread badge using a *private* read
  receipt, so the server's counter resets without telling anyone you read it.
- **Read receipts.** Small avatars under a message show who has read up to there (from
  their public receipts; people who use private receipts, like you, don't appear).
- **Room events.** Settings → *Show room events* adds joins, leaves, invites, kicks, bans,
  renames, description, picture and permission changes as grey lines in the timeline.
- **Instant sending.** Your message appears at once (dimmed, "Sending…") and settles when
  the server confirms it; if sending fails, the text goes back in the message box.
- **Public rooms** (the **#** button): search your server's or any other server's room
  directory, or join by address like `#room:matrix.org`. If your server won't fetch another
  server's directory, Whisker asks that server directly (anonymously, but it sees your IP
  unless the account uses a proxy).
- **Create rooms** (the **+** button above the room list): private and encrypted, or public
  with an address; invite people as you create it.
- **Profiles**: click anyone's picture (in chat or the member list) to open their profile
  in a side panel: picture, name, address, role, verified status, **Message**, **Verify**,
  mention, copy address, moderation and ignore.
- **Verification**: the emoji comparison, for other people and for this session against your
  other sessions (Settings → account → *Verify this session*); incoming requests pop up.
- **Room info** (click the room name or **i**, opens in the side panel): description, pinned messages (where rooms
  usually keep their rules), who can join, history visibility, encryption, the member list
  (click anyone to see their profile), inviting people, and editing the name/description if
  you're allowed.
- **Your profile**: change your display name and picture in Settings → your account.
- **Right-click a message** to reply, quote, copy its text, select part of it, save its
  file, copy a link to it, edit or delete it, or kick/ban its sender if you're a moderator.
- **Settings page** (⚙): dark (the default), light or follow-system theme; auto-lock after
  5/15/60 minutes idle; change a vault's passphrase; nickname, tags and proxy per account;
  a full AI agent setup form (model, API key, effort, permissions, commanders, audit room).
- **End-to-end encryption.** Built on matrix-rust-sdk. Each account's local store
  (room state and encryption keys) is kept *inside its vault file*, not in a folder of
  its own: while a vault is unlocked the store runs from RAM, and it is packed back into
  the vault every 10 minutes and when you lock or quit. Caches (timeline, media) stay
  in RAM and are rebuilt from the server.
- **Owner-only files.** On Linux and Android the data folder and everything in it are
  readable by your user only (folders 0700, files 0600), including installs made by
  older versions. Older versions kept stores in a `stores` folder; each one moves into
  its vault (and is deleted) the first time that vault is unlocked.

## Hidden vaults on disk

On disk, Whisker keeps nothing but `settings.json` (theme and lock timer) and the
`vaults` folder. Each vault file holds its accounts *and* their Matrix stores, so there
is no per-account file or folder to count.

The `vaults` folder always holds more files than you have vaults. Whisker adds *decoys*
on first run, whenever a vault is created, and now and then on unlock. A decoy has the
same header and key-derivation cost as a real vault and a size from the same steps
(powers of two from 64 KiB; decoys go up to 16 MiB); its contents are random, so
without a passphrase nobody can tell it from a vault, and "Whisker made those" is always
a true answer. A vault is rewritten in place, so it keeps its creation time, and after
every write all files get the same timestamps, so nothing shows which one is in use.
Unlocking tries every file, so the time it takes doesn't reveal anything either.

While a vault is unlocked, its stores run from a RAM-backed folder (`$XDG_RUNTIME_DIR`
or `/dev/shm` on Linux), which is wiped when you lock. If Whisker crashes, that copy
stays in RAM until you log out or reboot, and is used on the next unlock so nothing is
lost. On Windows the running copy is on disk while unlocked (wiped on lock); on Android
it is in the app's private storage.

What this doesn't hide:

- **Size.** A vault whose accounts are in many big rooms can grow past the largest decoy
  size (16 MiB), and an unusually large file is probably a real vault.
- **Deep forensics.** Filesystem journals, SSD wear-levelling, backups and snapshots can
  keep older copies or timings that no app can control. Use full-disk encryption.
- **Your servers.** A homeserver knows which accounts log in from the same IP unless you
  give them separate proxies.

## AI permissions

Before the AI can kick, ban or take any other moderation action, three checks must pass:

1. **Grant.** You must have given the agent that permission, either everywhere or for a
   specific room. A per-room list *replaces* the global one.
2. **Authority.** The person asking must be a *commander*: either at or above a power level
   (default 50), or on your list of named users. Anyone else can only chat with the AI,
   and the AI is never even offered the moderation tools in that conversation. (You can turn
   on `autonomous` mode if you want the AI to act on its own, for example against spam.)
3. **Server.** The homeserver checks Matrix power levels as usual.

On top of these checks, the AI can never act against itself, any of your own accounts, or
anyone on the protected list. It can't give anyone a power level above `max_grant_level`
or at or above its own. Each action is logged and can also be posted to an audit room.
Chat messages reach the model as JSON strings, one per line, and the request it's
answering (who asked, and whether they're a moderator) comes separately from the app.
A message can't close the transcript, fake another person's line or pretend to be the
request, however it's written. Tools are offered only for what the real sender may do,
so a message that says "ignore your rules and ban X" can't give anyone extra rights.
Language models can still be talked into things, though: if a moderator talks to the AI
right after someone posts instructions, the model could misread whose request it is.
Only grant moderation to agents in rooms where you're comfortable with that, and keep the
audit room on.

## Layout

```
crates/whisker-core    accounts, vaults, moderation, server extensions, timeline
crates/whisker-agent   AI agents: providers, permissions, tools, runner
apps/whisker           Slint GUI (desktop + Android)
apps/whisker-agentd    headless CLI: manage accounts/agents, run agents on a server
```

## Build

```sh
cargo build --release            # Linux / Windows
./target/release/whisker
```

Use the release build for real use; debug builds are much slower (dependencies are
optimised in debug too, but Whisker's own code isn't).

Android needs the NDK and [`xbuild`](https://github.com/rust-mobile/xbuild) or `cargo-apk`:

```sh
rustup target add aarch64-linux-android
x build -p whisker --platform android --arch arm64
```

## Setting up an AI account

1. In the GUI, click **+**, sign in to the bot's account, and tick
   *Automated AI account*. Or use `whisker-agentd login --agent …`.
2. Whisker opens its settings page. Paste the API address (e.g. `openrouter.ai/api` or
   `localhost:11434`), add a key if it needs one, press **Connect**, pick a model, set its
   permissions, then **Test** and **Save**. Each AI has its own page under *Settings → AI*.

From a shell instead:

```sh
whisker-agentd accounts                       # find the AI account's id
export AI_KEY=...
whisker-agentd agent add --account <id> --api-url openrouter.ai/api --api-key-env AI_KEY
                                               # lists the models; then add --model <name>
```

Agents start when you unlock the vault in the GUI. To keep one running all the time, run
`whisker-agentd run` on a server instead (not both at once on the same data directory).

The agent joins rooms when a commander or one of your own accounts invites it. It replies
when someone mentions it and to every DM (`--trigger every` makes it read every message).

## Status

This is an early version. It already does: vaults, signing in to several accounts at once,
tags, hidden vaults, a settings page, sync, room list and history, sending messages, moderation commands,
AI agents, and plugin discovery. Still to do: cross-signing and device verification, media, threads and reactions, notifications,
UI for plugin features, and an Android package that has been tested on a device.
