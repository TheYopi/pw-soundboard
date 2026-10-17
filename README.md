<p align="center">
  <img src="assets/pw-soundboard.png" width="128" alt="PW Soundboard icon">
</p>

# PW Soundboard

A soundboard for Linux. Click a tile and the sound plays **in your headphones** and **through
your microphone**, so your friends on Discord, viewers on OBS or people in a browser call hear it
too.

- Works with any app that uses a microphone, with no extra plugins or virtual cables.
- Your voice and the sounds go out together.
- Runs quietly in the system tray.
- Global keyboard shortcuts for your favourite sounds.
- Matches your desktop's colors, and follows them live when you switch themes.
- Optional: trigger sounds from an Elgato Stream Deck through
  [StreamController](https://github.com/StreamController/StreamController).

It's made for Linux desktops that use **PipeWire**. Most current distributions (Arch, Fedora,
Ubuntu 22.10+, CachyOS, and others) do.

---

## Getting started

1. Download the latest `PW_Soundboard-…-x86_64.AppImage` file from the
   [**Releases**](../../releases) page.
2. Make it runnable: right-click the file → *Properties* → *Permissions* → tick
   *Allow executing as program*. Or, in a terminal:
   ```bash
   chmod +x PW_Soundboard-*.AppImage
   ```
3. Double-click it to start.

The window opens with an empty board, and a PWSB icon appears in your system tray.

### Automatic updates with Gear Lever (recommended)

[Gear Lever](https://flathub.org/apps/it.mijorus.gearlever) installs AppImages as normal apps
(with a menu entry) and keeps them up to date. Open the downloaded AppImage with Gear Lever and
add it to your app menu. The update settings are filled in automatically; if
you ever need to enter them yourself, under **Update management** use:

| Field | Value |
|---|---|
| Username/Repo | `TheYopi/pw-soundboard` |
| Release file name | `PW_Soundboard-*-x86_64.AppImage.zsync` |

Gear Lever then shows when a new version is available and only downloads the parts that
changed.

---

## Adding sounds

- Click the **+ Add sound** tile and pick one or more audio files, **or**
- Drag audio files from your file manager straight onto the window.

Most formats work: MP3, WAV, FLAC, OGG, Opus, M4A/AAC, AIFF and more.

> The app remembers *where* your files are; it doesn't make a copy. If you move or delete an
> audio file later, select the sound and use **📂 Replace audio…** to point it at the new
> location.

### Giving a sound a picture, name or volume

Clicking a tile plays it and also opens its editor on the right. To edit without playing it,
right-click the tile → **✏ Edit…**. In the editor you can:

| What | How |
|---|---|
| Rename it | Type in **Title** |
| Add a picture | Click **🖼 Image…**, or drag an image file onto the window while the sound is selected |
| Make it louder or quieter | Move the **Volume** slider (changes apply right away, even while playing) |
| Swap the audio file | **📂 Replace audio…** |
| Delete it from the board | **🗑 Remove** (your audio file itself is *not* deleted) |

Right-clicking a tile also offers **▶ Play**, **⏹ Stop**, **🖼 Set image…** and **🗑 Remove**.

---

## Playing sounds

- **Click a tile** to play it. Click it again while it plays to stop it.
- **⏹ Stop all** (or the `Esc` key) stops everything.
- The slider in the top bar is the overall volume.
- **🔊 Speakers** and **🎤 Mic** in the top bar choose where sounds go: to you, to others, or
  both.
- Type in the search box to find a sound quickly.

---

## Letting others hear the sounds

Open **⚙ Routing** (top right). Under **Microphone output** there are two ways to do it.

### Option A: "Soundboard Virtual Mic" (default, works for everyone)

The app creates a new microphone called **Soundboard Virtual Mic**.

1. In Discord, OBS, your browser or game, set the **input device / microphone** to
   **Soundboard Virtual Mic**.
2. Back in the soundboard, tick **Mix my real microphone into it** so people still hear your
   voice. If needed, pick your real microphone under **Real microphone**.

### Option B: Mix into a microphone you already use (for EasyEffects users)

If you already run your microphone through a virtual device, for example **EasyEffects**,
choose it in **Microphone output** (e.g. *Easy Effects Source*). The sounds are mixed straight
into it, and you don't have to change anything in Discord or OBS.

The status bar at the bottom shows whether an app is currently listening.

> **Discord tip:** Discord's noise suppression (Krisp) treats sound effects and music as noise
> and may cut them. Set it to *Standard* or turn it off under *Voice & Video* if sounds are
> cut short or missing.

---

## Keyboard shortcuts

Give any sound a key combination, like `Ctrl+Alt+1`. It works on any desktop, even while you're
in a game or another app and the soundboard is hidden in the tray.

### One-time setup

Shortcuts need permission to read the keyboard. Open a terminal, run this once, then **log out
and back in**:

```bash
sudo usermod -aG input $USER
```

> This gives programs you run permission to read keyboard input. PW Soundboard only reacts to
> the combinations you set and doesn't record or store anything else.

### Setting a shortcut

1. Click a sound and find **Keyboard shortcut** in the editor.
2. Click **⌨ Set shortcut**.
3. Hold `Ctrl`, `Alt`, `Shift` and/or `Super` and press a key. `Esc` cancels.

That's it. To change or remove it, use **✏ Change** or **✖** next to the shortcut. A shortcut
for **Stop all sounds** is under **⚙ Routing → Behaviour**.

Good to know:
- Letters, numbers and other typing keys need at least one modifier, so they don't fire while
  you type. Keys like `F9` or media keys can be used on their own.
- The key press still reaches the app you're in. Pick combinations that app doesn't use.

---

## Appearance

Under **⚙ Settings → Appearance**:

- **Colors:** by default the soundboard uses your desktop's Qt color scheme (the files in
  `~/.config/qt6ct/colors` or `~/.config/qt5ct/colors`, which many desktop themes and shells
  generate). When your theme changes, the soundboard recolors itself within a second. You can
  also pick a specific scheme, or the built-in colors.
- **Shape:** *Angular* (cut corners, for that sci-fi look) or *Rounded*.
- **Tile size:** make the sound tiles bigger or smaller.

The app uses your system's default fonts.

---

## Running in the background (tray)

Closing the window doesn't quit the app. It keeps running in the system tray, and your sounds
keep working, including from a Stream Deck.

- **Left-click the tray icon** to open the window again.
- **Right-click the tray icon** for *Show Soundboard*, *Stop all sounds* and *Quit*.
- Starting the app again while it's already running just opens its window.

Under **⚙ Routing → Behaviour** you can turn this off (*Keep running in the tray when closed*) or
have it start straight into the tray (*Start hidden in the tray*). If your desktop has no
system tray, closing the window simply quits.

---

## Stream Deck (StreamController plugin)

If you use [StreamController](https://github.com/StreamController/StreamController) for an Elgato
Stream Deck, you can put your sounds on its buttons. The buttons show the sound's picture and
light up with a progress bar while it plays.

1. Download this repository (green **Code** button → *Download ZIP*) and unpack it.
2. Open a terminal in the unpacked folder and run:
   ```bash
   ./streamcontroller-plugin/install.sh
   ```
3. Restart StreamController.
4. Add **PW Soundboard → Play Sound** to a button and choose the sound in its settings. There's
   also a **Stop All Sounds** action.

All settings (volume, speakers/mic, overlap, and so on) stay in the soundboard app. The buttons
behave exactly like clicking the tiles. PW Soundboard must be running, in the window or the
tray.

---

## Troubleshooting

**Nobody hears the sounds.**
Check that your voice app's microphone is set to *Soundboard Virtual Mic* (Option A), or that
**Microphone output** points at the microphone that app uses (Option B). The status bar should
say *in use*. Also make sure **🎤 Mic** is switched on in the top bar.

**OBS records the sounds twice (echo).**
OBS records both your microphone and your desktop audio, so the sound arrives twice. In
**⚙ Routing → Speaker output**, pick your actual headphones/speakers instead of a virtual
device that OBS records as desktop audio. If you use EasyEffects, also add *PW Soundboard* to
its list of excluded apps.

**Keyboard shortcuts don't do anything.**
Open a sound's editor: under **Keyboard shortcut** the app tells you what's missing. Usually
it's the one-time setup (`sudo usermod -aG input $USER`) or logging out and back in afterwards.

**The file picker doesn't open.**
The file picker needs `xdg-desktop-portal` (installed on almost every desktop). You can always
drag and drop files onto the window instead.

**The AppImage won't start.**
Your system needs PipeWire, and AppImages need FUSE (`fuse3`, usually already installed). As a
fallback, run it with `--appimage-extract-and-run`.

---

## Where your data is stored

| What | Where |
|---|---|
| Your board and settings | `~/.config/pw-soundboard/config.json` |
| Pictures (small copies) | `~/.local/share/pw-soundboard/thumbnails/` |

---

## For developers

Native Rust app using egui and the PipeWire API directly, packaged as an AppImage.

```bash
# Arch Linux build dependencies
sudo pacman -S --needed rust pipewire wireplumber clang pkgconf

cargo run --release        # run from source
./build-appimage.sh        # → dist/PW_Soundboard-<version>-x86_64.AppImage
```

How it works, briefly:

- **Soundboard Virtual Mic** is a `support.null-audio-sink` node with
  `media.class = Audio/Source/Virtual`, owned by the app. A playback stream ("mic feed") is
  linked into its input ports by the app itself. Alternatively the feed is linked into any other
  virtual source with input ports (*Microphone output*).
- Mic passthrough always records an explicit device, never the virtual mic itself, so it can't
  feed back.
- `libpipewire` is deliberately not bundled in the AppImage; it must match the system's PipeWire.
- A local control socket (`~/.local/share/pw-soundboard/control.sock`, JSON lines) lets other
  programs list and trigger sounds; the StreamController plugin and the command line use it
  (`--play <number|title>`, `--stop`, `--stop-all`, `--help`).
- Keyboard shortcuts read the keyboards directly via evdev (`/dev/input`, needs the `input`
  group), because Wayland apps can't see other windows' keys and the GlobalShortcuts portal isn't
  available on every compositor. Keys are observed, never grabbed.
- The tray icon is a StatusNotifierItem (via `ksni`, no GTK).

```
src/main.rs     startup, window/tray lifecycle
src/app.rs      user interface
src/audio.rs    PipeWire engine: streams, virtual mic, linking, mixer
src/decode.rs   audio decoding + resampling to 48 kHz stereo
src/config.rs   settings and library (JSON)
src/remote.rs   control socket
src/shell.rs    tray icon, background mode
src/hotkeys.rs  keyboard shortcuts (evdev listener, hot-plug, recording)
src/theme.rs    colors from qt5ct/qt6ct schemes (live reload), fonts, shapes
streamcontroller-plugin/   Stream Deck plugin for StreamController
```

## License

MIT, see [LICENSE](LICENSE).
