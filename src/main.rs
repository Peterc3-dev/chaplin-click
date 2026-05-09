// chaplin-click — bridge between Chaplin lip-reading and screen-click.
//
// Workflow:
//   1. Snapshot existing /dev/input/event* devices
//   2. Launch Chaplin (spawns a pynput uinput virtual keyboard)
//   3. Watch /dev/input/ via inotify for the new event device
//   4. Launch screen-click --daemon, piping its stdin
//   5. Read key events from the virtual keyboard, buffer into sentences
//   6. On sentence boundary (. ? ! followed by space), feed to screen-click
//   7. Clean shutdown on SIGINT/SIGTERM (kill both children)
//
// Requires: user in `input` group (for /dev/input/* access).
// Hard rule: no ROCm, no Chaplin modification, no screen-click modification.

use anyhow::{bail, Context, Result};
use evdev::{Device, InputEventKind, Key};
use inotify::{Inotify, WatchMask};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------- phosphor palette ----------
const C_BRIGHT: &str = "\x1b[38;2;0;255;200m";
const C_DIM: &str = "\x1b[38;2;0;200;160m";
const C_FAINT: &str = "\x1b[38;2;0;130;100m";
const C_WARN: &str = "\x1b[38;2;255;200;0m";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

fn log(tag: &str, msg: &str) {
    eprintln!("  {C_BRIGHT}>{RESET} {C_DIM}{:<14}{RESET} {}", tag, msg);
}

fn warn(tag: &str, msg: &str) {
    eprintln!("  {C_WARN}!{RESET} {C_WARN}{:<14}{RESET} {}", tag, msg);
}

/// Map evdev Key to a character. Returns None for non-printable keys.
/// Handles shifted characters via the `shifted` flag.
fn key_to_char(key: Key, shifted: bool) -> Option<char> {
    match key {
        Key::KEY_A => Some(if shifted { 'A' } else { 'a' }),
        Key::KEY_B => Some(if shifted { 'B' } else { 'b' }),
        Key::KEY_C => Some(if shifted { 'C' } else { 'c' }),
        Key::KEY_D => Some(if shifted { 'D' } else { 'd' }),
        Key::KEY_E => Some(if shifted { 'E' } else { 'e' }),
        Key::KEY_F => Some(if shifted { 'F' } else { 'f' }),
        Key::KEY_G => Some(if shifted { 'G' } else { 'g' }),
        Key::KEY_H => Some(if shifted { 'H' } else { 'h' }),
        Key::KEY_I => Some(if shifted { 'I' } else { 'i' }),
        Key::KEY_J => Some(if shifted { 'J' } else { 'j' }),
        Key::KEY_K => Some(if shifted { 'K' } else { 'k' }),
        Key::KEY_L => Some(if shifted { 'L' } else { 'l' }),
        Key::KEY_M => Some(if shifted { 'M' } else { 'm' }),
        Key::KEY_N => Some(if shifted { 'N' } else { 'n' }),
        Key::KEY_O => Some(if shifted { 'O' } else { 'o' }),
        Key::KEY_P => Some(if shifted { 'P' } else { 'p' }),
        Key::KEY_Q => Some(if shifted { 'Q' } else { 'q' }),
        Key::KEY_R => Some(if shifted { 'R' } else { 'r' }),
        Key::KEY_S => Some(if shifted { 'S' } else { 's' }),
        Key::KEY_T => Some(if shifted { 'T' } else { 't' }),
        Key::KEY_U => Some(if shifted { 'U' } else { 'u' }),
        Key::KEY_V => Some(if shifted { 'V' } else { 'v' }),
        Key::KEY_W => Some(if shifted { 'W' } else { 'w' }),
        Key::KEY_X => Some(if shifted { 'X' } else { 'x' }),
        Key::KEY_Y => Some(if shifted { 'Y' } else { 'y' }),
        Key::KEY_Z => Some(if shifted { 'Z' } else { 'z' }),
        Key::KEY_0 => Some(if shifted { ')' } else { '0' }),
        Key::KEY_1 => Some(if shifted { '!' } else { '1' }),
        Key::KEY_2 => Some(if shifted { '@' } else { '2' }),
        Key::KEY_3 => Some(if shifted { '#' } else { '3' }),
        Key::KEY_4 => Some(if shifted { '$' } else { '4' }),
        Key::KEY_5 => Some(if shifted { '%' } else { '5' }),
        Key::KEY_6 => Some(if shifted { '^' } else { '6' }),
        Key::KEY_7 => Some(if shifted { '&' } else { '7' }),
        Key::KEY_8 => Some(if shifted { '*' } else { '8' }),
        Key::KEY_9 => Some(if shifted { '(' } else { '9' }),
        Key::KEY_SPACE => Some(' '),
        Key::KEY_DOT => Some(if shifted { '>' } else { '.' }),
        Key::KEY_COMMA => Some(if shifted { '<' } else { ',' }),
        Key::KEY_SEMICOLON => Some(if shifted { ':' } else { ';' }),
        Key::KEY_APOSTROPHE => Some(if shifted { '"' } else { '\'' }),
        Key::KEY_SLASH => Some(if shifted { '?' } else { '/' }),
        Key::KEY_MINUS => Some(if shifted { '_' } else { '-' }),
        Key::KEY_EQUAL => Some(if shifted { '+' } else { '=' }),
        Key::KEY_LEFTBRACE => Some(if shifted { '{' } else { '[' }),
        Key::KEY_RIGHTBRACE => Some(if shifted { '}' } else { ']' }),
        Key::KEY_BACKSLASH => Some(if shifted { '|' } else { '\\' }),
        Key::KEY_GRAVE => Some(if shifted { '~' } else { '`' }),
        _ => None,
    }
}

/// List current /dev/input/event* devices.
fn list_event_devices() -> Result<HashSet<PathBuf>> {
    let mut devices = HashSet::new();
    let input_dir = Path::new("/dev/input");
    if input_dir.exists() {
        for entry in fs::read_dir(input_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("event") {
                devices.insert(entry.path());
            }
        }
    }
    Ok(devices)
}

/// Wait for a new event device to appear in /dev/input/ using inotify.
/// Returns the path of the new device.
fn wait_for_new_device(
    existing: &HashSet<PathBuf>,
    timeout: Duration,
    shutdown: &Arc<AtomicBool>,
) -> Result<Option<PathBuf>> {
    let mut inotify = Inotify::init().context("inotify init")?;
    inotify
        .watches()
        .add("/dev/input", WatchMask::CREATE)
        .context("inotify watch /dev/input")?;

    // Set inotify fd to non-blocking
    {
        let fd = inotify.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }
    }

    let start = Instant::now();
    let mut buf = [0u8; 4096];

    // Check for new devices that appeared between snapshot and inotify setup
    let current = list_event_devices()?;
    for dev in current.difference(existing) {
        return Ok(Some(dev.clone()));
    }

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if start.elapsed() > timeout {
            return Ok(None);
        }

        // Poll with a short timeout to allow checking shutdown flag
        match inotify.read_events(&mut buf) {
            Ok(events) => {
                for event in events {
                    if let Some(name) = event.name {
                        let name_str = name.to_string_lossy();
                        if name_str.starts_with("event") {
                            let path = Path::new("/dev/input").join(&*name_str);
                            if !existing.contains(&path) {
                                return Ok(Some(path));
                            }
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Verify a device looks like a pynput/uinput virtual keyboard.
fn is_virtual_keyboard(path: &Path) -> bool {
    match Device::open(path) {
        Ok(dev) => {
            let name = dev.name().unwrap_or("");
            // pynput creates devices with names like "py-evdev-uinput"
            // ydotool creates "ydotoold virtual device"
            // We want the pynput one specifically
            let name_lower = name.to_lowercase();
            (name_lower.contains("uinput") || name_lower.contains("virtual"))
                && !name_lower.contains("ydotool")
        }
        Err(_) => false,
    }
}

fn kill_child(child: &mut Child, name: &str) {
    // Check if already exited before sending signals
    match child.try_wait() {
        Ok(Some(_)) => {
            log(name, "already exited");
            return;
        }
        Err(_) => return,
        Ok(None) => {}
    }
    // Use Child::kill (SIGKILL via handle, no PID reuse risk) with
    // a SIGTERM grace period first. Child::kill is safe because it
    // uses the kernel handle, not a raw PID.
    //
    // Unfortunately std::process::Child has no SIGTERM method, only
    // kill (SIGKILL). We do a polite wait first, then escalate.
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                log(name, "exited cleanly");
                return;
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(2) {
                    let _ = child.kill();
                    let _ = child.wait();
                    warn(name, "force-killed after timeout");
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return,
        }
    }
}

fn main() {
    let shutdown = Arc::new(AtomicBool::new(false));

    // Register signal handlers
    let shutdown_sig = shutdown.clone();
    signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown_sig.clone())
        .expect("register SIGINT");
    signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown_sig)
        .expect("register SIGTERM");

    eprintln!(
        "\n{C_BRIGHT}{BOLD}  chaplin-click{RESET}{C_FAINT}  lip-read -> screen-click bridge{RESET}\n"
    );

    match run(&shutdown) {
        Ok(()) => {
            eprintln!("{C_DIM}chaplin-click stopped.{RESET}");
        }
        Err(e) => {
            eprintln!("{RESET}{C_WARN}error:{RESET} {e:#}");
            std::process::exit(1);
        }
    }
}

fn run(shutdown: &Arc<AtomicBool>) -> Result<()> {
    // Check user has access to /dev/input
    let test_devices = list_event_devices().context("listing /dev/input")?;
    if test_devices.is_empty() {
        bail!(
            "no /dev/input/event* devices found.\n\
             Ensure user is in the 'input' group: sudo usermod -aG input $USER\n\
             Then log out and back in."
        );
    }

    // Try to open one device to check permissions
    let test_dev = test_devices.iter().next().unwrap();
    if Device::open(test_dev).is_err() {
        bail!(
            "cannot open {}: permission denied.\n\
             Add user to 'input' group: sudo usermod -aG input $USER\n\
             Then log out and back in.",
            test_dev.display()
        );
    }

    // Resolve paths
    let chaplin_dir = PathBuf::from("/home/raz/tools/chaplin");
    let chaplin_script = chaplin_dir.join("start.sh");
    if !chaplin_script.exists() {
        bail!("Chaplin not found at {}", chaplin_script.display());
    }

    let screen_click_bin = PathBuf::from("/home/raz/projects/screen-click/target/release/screen-click");
    if !screen_click_bin.exists() {
        bail!(
            "screen-click not found at {}\nBuild it: cd ~/projects/screen-click && cargo build --release",
            screen_click_bin.display()
        );
    }

    // Step 1: snapshot existing event devices
    let existing_devices = list_event_devices()?;
    log(
        "devices",
        &format!("{} existing event devices", existing_devices.len()),
    );

    // Step 2: launch screen-click in daemon mode
    log("screen-click", "starting daemon...");
    let mut screen_click = Command::new(&screen_click_bin)
        .arg("--daemon")
        .arg("--fast")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning screen-click --daemon")?;

    let mut sc_stdin = screen_click
        .stdin
        .take()
        .context("taking screen-click stdin")?;

    log("screen-click", "daemon started");

    // Step 3: launch Chaplin
    log("chaplin", "starting...");
    let mut chaplin = Command::new("bash")
        .arg(&chaplin_script)
        .current_dir(&chaplin_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning Chaplin")?;

    log("chaplin", "started, waiting for virtual keyboard device...");

    // Step 4: wait for the new virtual keyboard device from pynput
    let vkbd_path = match wait_for_new_device(&existing_devices, Duration::from_secs(60), shutdown)?
    {
        Some(p) => p,
        None => {
            if shutdown.load(Ordering::Relaxed) {
                kill_child(&mut chaplin, "chaplin");
                kill_child(&mut screen_click, "screen-click");
                return Ok(());
            }
            // No new device found — Chaplin might use a different backend or
            // the device appeared between snapshot and inotify. Do a final scan.
            let current = list_event_devices()?;
            let new_devs: Vec<_> = current.difference(&existing_devices).collect();
            if new_devs.is_empty() {
                warn("devices", "no new input device detected within 60s");
                warn(
                    "fallback",
                    "switching to stdin passthrough mode (type commands manually)",
                );
                // Fall back to manual stdin mode
                return run_manual_mode(&mut sc_stdin, &mut chaplin, &mut screen_click, shutdown);
            }
            new_devs[0].clone()
        }
    };

    // Give the device a moment to initialize
    std::thread::sleep(Duration::from_millis(200));

    // Verify it's a virtual keyboard
    let dev_name = match Device::open(&vkbd_path) {
        Ok(d) => d.name().unwrap_or("unknown").to_string(),
        Err(e) => {
            warn(
                "device",
                &format!("can't open {}: {e} — falling back to stdin mode", vkbd_path.display()),
            );
            return run_manual_mode(&mut sc_stdin, &mut chaplin, &mut screen_click, shutdown);
        }
    };

    log(
        "device",
        &format!("found: {} ({})", vkbd_path.display(), dev_name),
    );

    if !is_virtual_keyboard(&vkbd_path) {
        warn(
            "device",
            &format!(
                "{} doesn't look like a pynput virtual keyboard (name='{}')",
                vkbd_path.display(),
                dev_name
            ),
        );
        warn("device", "proceeding anyway — may capture wrong input");
    }

    // Step 5: read key events and buffer sentences
    log("bridge", "reading key events, buffering sentences...");
    log(
        "bridge",
        "sentences (ending with ./?/! + space) will be sent to screen-click",
    );
    eprintln!(
        "\n{C_BRIGHT}{BOLD}  READY{RESET}{C_DIM}  mouth commands to your webcam. Alt to start/stop recording.{RESET}\n"
    );

    let mut device = Device::open(&vkbd_path)
        .with_context(|| format!("opening {}", vkbd_path.display()))?;

    // Set non-blocking so we can check shutdown flag
    {
        let fd = device.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            bail!("fcntl F_GETFL failed");
        }
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if rc < 0 {
            bail!("fcntl F_SETFL O_NONBLOCK failed");
        }
    }

    let mut buffer = String::new();
    let mut shift_held = false;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Check if Chaplin is still running
        match chaplin.try_wait() {
            Ok(Some(status)) => {
                log("chaplin", &format!("exited with {status}"));
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }

        // Check if screen-click is still running
        match screen_click.try_wait() {
            Ok(Some(status)) => {
                warn("screen-click", &format!("daemon exited with {status}"));
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }

        // Read events from the virtual keyboard
        let mut pipe_broken = false;
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    if pipe_broken {
                        break;
                    }
                    match event.kind() {
                        InputEventKind::Key(key) => {
                            let value = event.value(); // 0=release, 1=press, 2=repeat

                            // Track shift state
                            if key == Key::KEY_LEFTSHIFT || key == Key::KEY_RIGHTSHIFT {
                                shift_held = value != 0;
                                continue;
                            }

                            // Only process key-press events (value == 1)
                            if value != 1 {
                                continue;
                            }

                            // Handle backspace
                            if key == Key::KEY_BACKSPACE {
                                buffer.pop();
                                continue;
                            }

                            // Handle enter — treat as sentence end
                            if key == Key::KEY_ENTER {
                                let sentence = buffer.trim().to_string();
                                if !sentence.is_empty() {
                                    log("captured", &sentence);
                                    if let Err(e) = writeln!(sc_stdin, "{}", sentence) {
                                        warn("pipe", &format!("write to screen-click failed: {e}"));
                                        pipe_broken = true;
                                        break;
                                    }
                                    if let Err(e) = sc_stdin.flush() {
                                        warn("pipe", &format!("flush failed: {e}"));
                                        pipe_broken = true;
                                        break;
                                    }
                                }
                                buffer.clear();
                                continue;
                            }

                            // Map key to character
                            if let Some(ch) = key_to_char(key, shift_held) {
                                buffer.push(ch);

                                // Check for sentence boundary: ends with ./?/! followed by space
                                // Chaplin always appends a space after the sentence-ending punctuation
                                if ch == ' ' && buffer.len() >= 2 {
                                    let trimmed = buffer.trim();
                                    if let Some(last_non_space) = trimmed.chars().last() {
                                        if last_non_space == '.' || last_non_space == '?'
                                            || last_non_space == '!'
                                        {
                                            let sentence = trimmed.to_string();
                                            log("captured", &sentence);

                                            if let Err(e) = writeln!(sc_stdin, "{}", sentence) {
                                                warn(
                                                    "pipe",
                                                    &format!(
                                                        "write to screen-click failed: {e}"
                                                    ),
                                                );
                                                pipe_broken = true;
                                                break;
                                            }
                                            if let Err(e) = sc_stdin.flush() {
                                                warn("pipe", &format!("flush failed: {e}"));
                                                pipe_broken = true;
                                                break;
                                            }
                                            buffer.clear();
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No events available, sleep briefly
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                warn("evdev", &format!("read error: {e}"));
                // Device might have been removed, check if Chaplin died
                std::thread::sleep(Duration::from_millis(500));
            }
        }

        if pipe_broken {
            warn("pipe", "screen-click pipe broken, shutting down");
            break;
        }
    }

    // Cleanup
    log("shutdown", "stopping children...");
    kill_child(&mut chaplin, "chaplin");
    drop(sc_stdin); // Close stdin pipe to signal screen-click to exit
    kill_child(&mut screen_click, "screen-click");

    Ok(())
}

/// Fallback mode: user types commands manually into this terminal,
/// they get forwarded to screen-click. Chaplin still runs and types
/// into whatever window has focus.
fn run_manual_mode(
    sc_stdin: &mut dyn Write,
    chaplin: &mut Child,
    screen_click: &mut Child,
    shutdown: &Arc<AtomicBool>,
) -> Result<()> {
    eprintln!(
        "\n{C_BRIGHT}{BOLD}  MANUAL MODE{RESET}{C_DIM}  type target descriptions here, one per line.{RESET}"
    );
    eprintln!(
        "{C_DIM}  Chaplin runs separately — its output goes to the focused window.{RESET}\n"
    );

    let stdin = std::io::stdin();
    let mut line = String::new();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "quit" || trimmed == "exit" {
                    break;
                }
                if let Err(e) = writeln!(sc_stdin, "{}", trimmed) {
                    warn("pipe", &format!("write to screen-click failed: {e}"));
                    break;
                }
                if let Err(e) = sc_stdin.flush() {
                    warn("pipe", &format!("flush failed: {e}"));
                    break;
                }
            }
            Err(e) => {
                warn("stdin", &format!("read error: {e}"));
                break;
            }
        }
    }

    log("shutdown", "stopping children...");
    kill_child(chaplin, "chaplin");
    kill_child(screen_click, "screen-click");
    Ok(())
}
