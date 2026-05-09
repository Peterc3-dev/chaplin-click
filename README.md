# chaplin-click

Bridge between [Chaplin](~/tools/chaplin/) lip-reading and [screen-click](~/projects/screen-click/) for mutism-friendly mouse control.

Mouth a description of a UI element at your webcam. Chaplin reads your lips and produces text. This bridge captures the synthetic keystrokes from Chaplin's pynput output via evdev, buffers them into sentences, and feeds them to screen-click's daemon mode, which finds and clicks the matching element.

## How it works

1. Snapshots existing `/dev/input/event*` devices
2. Launches Chaplin (which creates a pynput uinput virtual keyboard)
3. Watches for the new virtual keyboard device via inotify
4. Launches screen-click in `--daemon --fast` mode
5. Reads key events from the virtual keyboard only (ignoring real keyboard input)
6. Buffers characters into sentences (boundary: `.` / `?` / `!` + space)
7. Writes completed sentences to screen-click's stdin

## Prerequisites

- User in `input` group: `sudo usermod -aG input $USER` (log out + in)
- Ollama running with `qwen3:4b` pulled (for Chaplin's LLM cleanup)
- Qwen2.5-VL server on `:8082` (for screen-click's VLM grounding)
- `ydotoold` running (for screen-click's mouse clicks)
- Webcam at `/dev/video0`

## Usage

```sh
./start.sh
```

Or launch from the desktop: **Chaplin Click** in Phosphor Tools.

Once running, press **Alt** to start recording, mouth your command (e.g. "click the settings icon"), press **Alt** again to stop. Chaplin lip-reads it, the bridge captures the output, and screen-click finds and clicks the element.

Press **q** on the camera window to exit (shuts down everything cleanly).

### Fallback mode

If no virtual keyboard device is detected (e.g. pynput uses a different backend), the bridge falls back to manual stdin mode where you type target descriptions directly.

## Build

```sh
cargo build --release
```

## Signals

SIGINT (Ctrl-C) and SIGTERM both trigger clean shutdown: Chaplin and screen-click are terminated gracefully.
