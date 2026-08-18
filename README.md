<div align="center">

<sub>HOLDTAP · FOR LINUX TOUCHPADS</sub>

# Keep your finger down.

**Move with one finger. Tap with another. Click without breaking your flow.**

HoldTap adds one small gesture that many touchpads are missing.

[How it feels](#the-gesture) · [Install](#install) · [Tune](#tune-the-gesture) · [How it works](#how-it-works)

</div>

---

## The gesture

Keep one finger on the touchpad and move the pointer as usual. Briefly touch the
pad with a second finger to click the left mouse button. You never have to lift
the finger that controls the pointer.

The gesture stays out of the way:

- **Tap** the second finger to click.
- **Move** the second finger to start two-finger scrolling at once.
- **Hold** the second finger for 140 ms to switch to scrolling.
- **Add** a third or fourth finger to pass the full gesture to your compositor.

There is no compositor patch and no modified libinput package.

## Install

### Arch Linux

```bash
git clone https://github.com/wojciechsacewicz/holdtap.git
cd holdtap/packaging
makepkg -si
sudo systemctl enable --now holdtap.service
```

### Other distributions

You need a current Rust toolchain and Linux headers with `uinput` support.

```bash
cargo build --release --locked
sudo install -Dm755 target/release/holdtap /usr/local/bin/holdtap
sudo ./target/release/holdtap
```

For everyday use, copy the included service file and adjust `ExecStart` if you
install the binary under `/usr/local/bin`.

> The daemon needs permission to read `/dev/input/event*`, grab the physical
> touchpad, and create `/dev/uinput`. The included system service runs as root
> and applies systemd hardening options.

## Tune the gesture

The defaults come from real touchpad traces and aim to make scrolling wake up
quickly without turning a short tap into a scroll.

```text
holdtap — tap a second finger to click without lifting the first

Usage: holdtap [OPTIONS]

Options:
  -d, --device PATH|NAME    Use this evdev path or exact device name
      --tap-timeout-ms N    Restore scrolling after N ms [default: 140]
      --scroll-move-mm N    Restore scrolling after N mm of movement [default: 0.8]
      --primary-age-ms N    Required age of the first contact [default: 45]
      --primary-move-mm N   Required movement of the first contact [default: 1.5]
```

Add options to `ExecStart` in a systemd override:

```bash
sudo systemctl edit holdtap.service
```

For example:

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/holdtap --tap-timeout-ms 120 --scroll-move-mm 0.7
```

## How it works

HoldTap sits between the physical touchpad and the normal Linux input
stack. It grabs the physical evdev device and exposes a matching virtual uinput
device.

When the first finger is already moving the pointer, the daemon briefly hides a
new second contact. A quick release emits `BTN_LEFT`. Movement, a timeout, or an
extra finger restores the hidden contact and lets libinput handle the gesture.

The kernel releases the device grab if the daemon stops or crashes. Your
touchpad then returns to its normal behavior.

## Compatibility

- Linux with evdev and uinput
- A multitouch touchpad that reports slots and tracking IDs
- Wayland or X11; the daemon works below the display server
- libinput-based desktops and compositors

The project is young and hardware varies. If automatic detection selects the
wrong device, pass its event path or exact name with `--device`.

## Stop or remove

```bash
sudo systemctl disable --now holdtap.service
```

On Arch Linux, remove the package with:

```bash
sudo pacman -Rns holdtap
```

## Safety and privacy

The daemon runs locally. It has no network code, telemetry, analytics, or event
logging. It reads touch contacts only to classify the gesture and does not save
input events.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## License

[MIT](LICENSE)
