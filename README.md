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
  Gemini, OpenAI, Venice, LM Studio, llama.cpp, vLLM, Ollama, or any Anthropic-style API).
  Whisker works out what kind of API it is, lists its models, and has a Test button. With
  Venice, its own system prompt and web search are always sent as *off* (web search would
  pass chat messages to a third-party search engine) unless you switch them on for that AI.
- **AI memory.** Each AI remembers the last messages of every room word for word (40 by
  default, saved in the vault so restarts don't wipe it) and folds older ones into a
  running summary, made by the chat model or a cheaper one you name. The prompt stays
  within a token budget (newest messages first, each capped at 2 KiB).
- **No bot loops.** An AI never answers your other AI accounts or `m.notice` messages
  (Matrix's marker for bot output), and after a few replies in quick succession it waits
  for a pause, so two bots in one room can't talk to each other forever. It answers a
  mention only when its name or address appears as a whole word.
- **Admin controls.** In the composer, type `/kick`, `/ban`, `/unban`, `/invite`, `/redact`,
  `/op @user 50` or `/deop`. The AI uses the same actions.
- **Server plugins.** If the homeserver runs hot-loaded WASM plugins (anoncat), Whisker
  can find them at runtime and call their methods (no UI uses them yet). See
  [docs/PLUGIN_PROTOCOL.md](docs/PLUGIN_PROTOCOL.md).
- **Chat:** Markdown formatting (bold, italic, strikethrough, `code`, code blocks, quotes,
  lists, headings, clickable links); replies, edits and deletions; image previews with a
  full-size viewer; file cards; date dividers; a multi-line composer (Enter sends,
  Shift+Enter adds a line); and an **attach** button for images and files, as big as your server takes (up to 2 GB),
  with the upload's progress in percent.
- **Emoji and reactions.** The ☺ button next to Send opens a searchable emoji picker.
  Reactions show under messages with a count (hover to see who); click one to add or take
  back your own, or right-click a message → *React*. Emoji are in colour everywhere, with
  nothing to install: Whisker carries Noto Emoji pictures (picker, reactions, suggestions,
  big emoji-only messages) and a trimmed copy of the Noto Color Emoji font, listed before
  the system fonts, for emoji inside text (see `apps/whisker/assets`).
- **Share to Whisker** from other apps: files, pictures, text and links. Whisker asks which
  account should send it (skipped if you only have one), then which room, sends it, and opens
  that room. Several files at once are fine. On FerroOS it's in the Share box; elsewhere use
  "Open with" or run `whisker FILE…`. If Whisker is already open, the share goes to that
  window; if it's locked, the share waits until you unlock. Starting a different (newer)
  build while an older one is open closes the old one, so the new one takes over. On
  Android it's in the system Share menu.
- **Forward** any message from its right-click menu to one or more of your rooms. Images
  and files are re-posted as the same file (nothing is downloaded or uploaded again);
  replies lose their reply link. Forwarding from an encrypted room into an unencrypted
  one makes that file readable to the new room.
- **File links:** *Copy file link* on a picture, video or file's menu copies its address
  (`mxc://server/id`). Paste one on its own and send it, and it goes out as that picture,
  video or file without being uploaded again; Whisker asks the server only for its type and
  size (and a picture's dimensions). A message that is only such a link (sent from another
  app as text) shows the file itself. Media download addresses
  (`https://…/_matrix/…/download/server/id`) work too. An encrypted file's address can't
  open it without the message's key, so its link is a matrix.to link to the message: sending
  that re-posts the file, for people who can see the original room.
- **Images and videos:** right-click the picture or video itself to save it; the full-size
  viewer has Save / Forward / Close buttons and its own right-click menu. **GIFs and
  animated WebP play and loop**, in the timeline and the viewer. Videos show their preview
  frame (when they were sent without one, and are under 200 MB, Whisker makes one with
  ffmpeg if it's installed; videos you send get one the same way). Click a video to **play it
  right there in the message**, with sound, pause, a seek bar and mute (on Linux, with ffmpeg
  installed; it plays through PulseAudio/PipeWire, else ALSA). One plays at a time, and it stops
  when you switch rooms. Without ffmpeg, and on other systems, it opens in mpv or your
  system's player. The copy goes to the RAM folder and is deleted when you lock or quit.
- **Spaces** show as folders in the room list, holding their rooms and sub-spaces (a folded
  space shows the unread count of everything inside). Clicking a space opens its page: every
  room it lists, with **Join** for the ones you aren't in yet and **Open** for the rest, plus
  **Invite people** and **Space info**. Click it again to fold or unfold it. A space you just
  joined shows as a space straight away (Whisker asks the server if its copy doesn't know).
  Right-click a room on the page to open or join it, see its info, invite people, mark it
  read, copy its address or leave it.
- **Autocorrect** fixes typos when you finish a word: common ones ("teh" → "the", "dont" →
  "don't", a lone "i" → "I"), and any word that isn't English but is one slip (swapped,
  extra, missing or wrong letter) from exactly one word that is: "thnig" → "thing",
  "messgae" → "message". When two words fit equally it leaves yours alone. Backspace
  straight after a fix keeps what you typed (and that word isn't fixed again). Turn it off
  in Settings. The word list comes from SCOWL (`apps/whisker/assets/words-LICENSE.txt`).
- **Right-click to cut, copy, paste or select all** in every text box and the composer, and
  to copy addresses, descriptions and pinned messages in the side panel.
- **Room keys:** Settings → account → *Export or import room keys* saves your message keys
  to a passphrase-locked file (the format Element uses), or reads one in.
- **Threads:** replies in a thread are tucked under the message that started it ("3
  replies"); click that, or right-click → *Reply in thread*, to read and answer in the side panel.
- **Mentions and emoji shortcodes:** type `@` and part of a name for the closest matches
  (↑/↓ move through them, Tab or Enter takes the highlighted one); picked people become real mentions, so they're notified,
  and mentions show in the accent colour like links (click one for that person's profile).
  `:smile:` turns into 😄 as you type the closing colon; `:smi` offers matches, and a
  whole-word `:smile` is converted when you send. The ☺ button's picker takes the keyboard
  too: type to search, arrows to move, Enter to insert, Escape to close. Replying, editing,
  quoting, picking an emoji or switching rooms puts the cursor back in the message box.
- **Clear messages (moderators):** Room info → *Clear messages…*, or type `/clear` on its own,
  deletes the last 50, last 500 or all messages in the room for everyone (after asking).
  The command can be renamed (e.g. `?clear`) or turned off in Settings → *Commands*.
- **Notifications** (Linux desktops) for direct messages, mentions, and rooms you've set to
  notify, never for the room you're looking at or for messages from your own accounts.
  Settings → *Notifications and typing* picks how much they say: nothing, who and where
  (the default), or the message too.
- **Typing:** "Bob is typing…" shows above the composer. Whisker only tells others that
  *you're* typing if you turn that on.
- **Drag to select** any part of a message and copy it with Ctrl+C (turn on *Rich message
  text* in Settings for bold/italic/clickable links instead; rich text can't be selected).
  Click a **name** to reply to that message; click an **avatar** to open the person's
  profile (direct chat, mention, copy id, kick/ban if you're a moderator, ignore).
- **Drag an account** up or down the rail to reorder; the others slide aside like tabs (or
  right-click it for Move up/down). **Double-click an account** to fold the room list away
  for more room to read, and again to bring it back. Room admins can **change a room's picture** from Room info. **Right-click a room** to
  pin it to the top (saved as a favourite, so other apps see it), mark it read, copy its
  address or leave it. Opening a room clears its unread badge using a *private* read
  receipt, so the server's counter resets without telling anyone you read it.
- **Read receipts.** Small avatars under a message show who has read up to there (from
  their public receipts; people who use private receipts, like you, don't appear). Click
  them for the list of readers; click a reader for their profile.
- **Repeated joins hidden.** Some servers don't say what a member event replaced, so every
  name or picture change looks like another join; Whisker hides those repeats.
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
  People you've verified get a green *✓ Verified* by their name in the chat, and a red
  *⚠ Identity changed* if their keys changed since. Encrypted messages from a device its
  owner never verified (or an unknown one) carry a ⚠, with the reason on hover, as in
  Element. A profile lists the person's sessions, verified ones first, and the account page
  says whether this session is verified.
- **No other session needed.** A new account gets its cross-signing keys on the device that
  creates or first logs into it, so that session is verified from the start (Settings →
  account → *Set up verification* does it for an account that has none yet). *Make a
  recovery key* turns on key backup; a later login can enter that key to verify itself and
  read older messages. Lost every session and the key? *Reset verification* starts over.
  Your AI accounts and your other accounts on this device verify each other automatically
  (but two people-accounts never do, so alts stay unlinked).
- **Room info** (click the room name or **i**, opens in the side panel): description, pinned messages (where rooms
  usually keep their rules), who can join, history visibility, encryption, the member list
  (click anyone to see their profile), inviting people, and editing the name/description if
  you're allowed.
- **Your profile**: change your display name and picture in Settings → your account.
- **Select text by dragging, like in a web browser:** from the middle of one message, over
  names and whole messages, to the middle of another (hold the mouse above or below the list
  to scroll while selecting). Ctrl+C or right-click → *Copy* copies it, names on their own lines.
- **Right-click a message** to react, reply, forward, copy it, open its links, save its file,
  edit or delete it, or kick/ban its sender if you're a moderator. Settings → *Message menu*
  also offers Reply in thread, Quote, Play / Open image, Copy link to message, View profile,
  Copy user id and Show source, which start off.
- **Settings page** (⚙): dark (the default), light or follow-system theme; auto-lock after
  5/15/60 minutes idle; change a vault's passphrase; nickname, tags and proxy per account;
  a full AI agent setup form (model, API key, effort, permissions, commanders, audit room).
- **AI accounts.** An AI chats as its own Matrix account. Add one from Settings → General
  → *Add an AI account*, or turn an account you have into one from its settings page. Its
  page under **AI** takes any API address and key (Venice, OpenAI, Anthropic, OpenRouter,
  a local server…), the model (filled in with the service's recommended one, Venice's own
  default or a well-liked quick model, with a *Use it* button to switch back), and Venice's privacy switches (both off by default).
  *Longest reply posted* caps replies in characters (cut at the last sentence that fits),
  and *Let the AI see pictures* (off by default) sends up to three of the newest pictures in the
  last dozen messages (and a picture someone replies to, forwarded ones included), shrunk to 1024 px, to models that can
  see images. The page says whether the chosen chat model can see; for one that can't, pick
  a *Picture model* from the dropdown of the service's models that can (or type one): it
  describes each picture in words for the chat model. Pictures posted before the AI last
  restarted are fetched again when it answers, and each is fetched only once. Saving warns when the chat model probably can't see and no picture model is set. An AI answers when it's named,
  mentioned, or someone replies to one of its messages or in a thread it started, and shows
  as typing while it writes. Someone talking to it directly is always answered; the
  rapid-exchange limit only holds back answers to its name in passing.
- **Character cards.** On an AI's page, *Import card…* reads Tavern / SillyTavern character
  cards, versions 1, 2 and 3, as `.json` or as `.png` (the card hidden in the picture,
  `ccv3` read before `chara`). The AI then plays that character: description,
  personality, scenario and example messages go into its instructions (your own text stays,
  and a card's system prompt can place it with `{{original}}`), `{{char}}` and `{{user}}`
  are filled in, lorebook entries are added when their keywords come up (constant,
  selective and case-sensitive entries and the scan depth are honoured, and so are the
  CCv3 `@@depth`, `@@role`, `@@position`, `@@activate` and `@@dont_activate` decorators and
  SillyTavern's "at depth" setting; no `@@` line ever reaches the model). Entries set to a
  depth and the card's post-history instructions go at the end of the conversation, after
  the message being answered, where reminders actually work. A `<state>…</state>` block in
  a reply is never posted, even cut off: it's kept and shown back to the model next time.
  It greets
  with the first message when it joins a room (can be turned off). Every field can be
  edited; *Export .png* / *Export .json* write it back out as CCv3 (plus v2 in the PNG, for
  older apps), keeping fields Whisker doesn't use. *Use the card's name and picture* sets
  the AI account's display name and avatar. Cards and their pictures are kept in the vault.
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

To have Whisker in the app menu and the FerroOS Share box, install its desktop entry (it
expects `whisker` on your `PATH`, e.g. linked into `~/.local/bin`):

```sh
ln -s "$PWD/target/release/whisker" ~/.local/bin/whisker
cp apps/whisker/whisker.desktop ~/.local/share/applications/
```

Android needs the NDK and [`xbuild`](https://github.com/rust-mobile/xbuild) or `cargo-apk`.
The share menu entry, permissions and package name are in `apps/whisker/Cargo.toml` under
`[package.metadata.android]` (cargo-apk's format):

```sh
rustup target add aarch64-linux-android
x build -p whisker --platform android --arch arm64
```

## Setting up an AI account

1. In the GUI, click **+**, sign in to the bot's account, and tick
   *Automated AI account*. Or use `whisker-agentd login --agent …`. An account you've
   already added can become an AI too: *Settings → its page → Use this account as an AI*.
2. Whisker opens its settings page. Paste the API address (e.g. `api.venice.ai/api/v1`, `openrouter.ai/api` or
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

This is an early version. It already does: vaults and hidden vaults, many accounts at once
with tags, spaces, rooms, history, threads, replies, edits, reactions, forwarding, media,
mentions, notifications, typing, read receipts, session verification, moderation, AI accounts
with character cards, and plugin discovery. Still to do: video and audio playback inside the app, notifications on Windows and macOS,
UI for plugin features, and an
Android package that has been tested on a device.
