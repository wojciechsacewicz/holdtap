mod recognizer;

use evdev::uinput::VirtualDevice;
use evdev::{AbsoluteAxisCode, Device, EventType, InputEvent, KeyCode, PropType, UinputAbsSetup};
use recognizer::{Config, Decision, Point, Recognizer};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const POLL_MS: i32 = 10;

#[derive(Debug)]
struct Options {
    device: Option<String>,
    config: Config,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut options = Self {
            device: None,
            config: Config::default(),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let value = |args: &mut std::iter::Skip<std::env::Args>| {
                args.next()
                    .ok_or_else(|| format!("missing value after {arg}"))
            };
            match arg.as_str() {
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("holdtap {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "-d" | "--device" => options.device = Some(value(&mut args)?),
                "--tap-timeout-ms" => {
                    options.config.tap_timeout =
                        Duration::from_millis(parse(&value(&mut args)?, &arg)?);
                }
                "--scroll-move-mm" => {
                    options.config.scroll_move_mm = parse(&value(&mut args)?, &arg)?
                }
                "--primary-age-ms" => {
                    options.config.min_primary_age =
                        Duration::from_millis(parse(&value(&mut args)?, &arg)?);
                }
                "--primary-move-mm" => {
                    options.config.primary_move_mm = parse(&value(&mut args)?, &arg)?
                }
                _ => return Err(format!("unknown option: {arg}")),
            }
        }
        Ok(options)
    }
}

fn parse<T: std::str::FromStr>(value: &str, option: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("invalid value for {option}: {value}"))
}

fn print_help() {
    println!(
        r#"holdtap — tap a second finger to click without lifting the first

Usage: holdtap [OPTIONS]

Options:
  -d, --device PATH|NAME    Use this evdev path or exact device name
      --tap-timeout-ms N    Restore scrolling after N ms [default: 140]
      --scroll-move-mm N    Restore scrolling after N mm of movement [default: 0.8]
      --primary-age-ms N    Required age of the first contact [default: 45]
      --primary-move-mm N   Required movement of the first contact [default: 1.5]
  -h, --help                Print help
  -V, --version             Print version"#
    );
}

#[derive(Clone, Copy, Default)]
struct SlotState {
    tracking_id: Option<i32>,
    x: Option<i32>,
    y: Option<i32>,
}

fn main() {
    let options = Options::parse().unwrap_or_else(|error| {
        eprintln!("holdtap: {error}\nTry --help for usage.");
        std::process::exit(2);
    });
    loop {
        match find_device(options.device.as_deref()) {
            Some((path, device)) => {
                eprintln!("holdtap: attaching to {}", path.display());
                if let Err(error) = run_device(device, options.config) {
                    eprintln!("holdtap: device stopped: {error}; retrying");
                }
            }
            None => eprintln!("holdtap: touchpad not found; retrying"),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn find_device(requested: Option<&str>) -> Option<(PathBuf, Device)> {
    evdev::enumerate().find(|(path, device)| {
        if device
            .name()
            .is_some_and(|name| name.ends_with(" (holdtap)"))
        {
            return false;
        }
        requested.map_or_else(
            || is_touchpad(device),
            |value| path == Path::new(value) || device.name() == Some(value),
        )
    })
}

fn is_touchpad(device: &Device) -> bool {
    let axes = device.supported_absolute_axes();
    device.properties().contains(PropType::POINTER)
        && !device.properties().contains(PropType::DIRECT)
        && axes.is_some_and(|axes| {
            axes.contains(AbsoluteAxisCode::ABS_MT_SLOT)
                && axes.contains(AbsoluteAxisCode::ABS_MT_POSITION_X)
                && axes.contains(AbsoluteAxisCode::ABS_MT_POSITION_Y)
                && axes.contains(AbsoluteAxisCode::ABS_MT_TRACKING_ID)
        })
}

fn run_device(mut source: Device, config: Config) -> io::Result<()> {
    let mut output = clone_device(&source)?;
    let resolution = source
        .get_absinfo()?
        .find(|(axis, _)| *axis == AbsoluteAxisCode::ABS_MT_POSITION_X)
        .map_or(12, |(_, info)| info.resolution());
    let mut recognizer = Recognizer::new(resolution, config);
    let mut slots = BTreeMap::<u16, SlotState>::new();
    let mut current_slot = 0_u16;
    let started = Instant::now();

    source.grab()?;
    source.set_nonblocking(true)?;
    eprintln!("holdtap: active (resolution {resolution} units/mm)");

    loop {
        wait_readable(&source, POLL_MS)?;
        let now = started.elapsed();
        let mut events = Vec::new();
        match source.fetch_events() {
            Ok(batch) => events.extend(batch),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }

        if events.is_empty() {
            if let Decision::RestoreForScroll(slot) = recognizer.tick(now) {
                restore_contact(&mut output, &recognizer, slot)?;
            }
            continue;
        }

        let mut frame = Vec::new();
        for event in events {
            if event.event_type() == EventType::SYNCHRONIZATION && event.code() == 0 {
                emit_frame(&mut output, &mut frame, &recognizer)?;
                continue;
            }

            let mut decision = Decision::Pass;
            if event.event_type() == EventType::ABSOLUTE {
                match event.code() {
                    code if code == AbsoluteAxisCode::ABS_MT_SLOT.0 => {
                        current_slot = event.value() as u16;
                    }
                    code if code == AbsoluteAxisCode::ABS_MT_TRACKING_ID.0 => {
                        if event.value() >= 0 {
                            slots.entry(current_slot).or_default().tracking_id =
                                Some(event.value());
                            decision = recognizer.touch_down(current_slot, event.value(), now);
                        } else {
                            decision = recognizer.touch_up(current_slot);
                            slots.remove(&current_slot);
                        }
                    }
                    code if code == AbsoluteAxisCode::ABS_MT_POSITION_X.0 => {
                        slots.entry(current_slot).or_default().x = Some(event.value());
                    }
                    code if code == AbsoluteAxisCode::ABS_MT_POSITION_Y.0 => {
                        slots.entry(current_slot).or_default().y = Some(event.value());
                    }
                    _ => {}
                }
                if let Some(state) = slots.get(&current_slot)
                    && let (Some(x), Some(y)) = (state.x, state.y)
                {
                    let position_decision = recognizer.position(current_slot, Point { x, y }, now);
                    if !matches!(position_decision, Decision::Pass | Decision::Hide(_)) {
                        decision = position_decision;
                    }
                }
            }

            if let Decision::RestoreForScroll(slot) = decision {
                restore_contact(&mut output, &recognizer, slot)?;
            }

            let pending = recognizer.pending_slot();
            let clicked_slot = if let Decision::Click(slot) = decision {
                Some(slot)
            } else {
                None
            };
            let hidden_slot_event = event.event_type() == EventType::ABSOLUTE
                && (pending == Some(current_slot)
                    || clicked_slot == Some(current_slot)
                    || (event.code() == AbsoluteAxisCode::ABS_MT_SLOT.0
                        && (pending == Some(event.value() as u16)
                            || clicked_slot == Some(event.value() as u16))));
            if !hidden_slot_event && !is_tool_count(event) {
                frame.push(event);
            }

            if matches!(decision, Decision::Click(_)) {
                emit_frame(&mut output, &mut frame, &recognizer)?;
                output.emit(&[InputEvent::new(EventType::KEY.0, KeyCode::BTN_LEFT.0, 1)])?;
                output.emit(&[InputEvent::new(EventType::KEY.0, KeyCode::BTN_LEFT.0, 0)])?;
            }
        }
    }
}

fn clone_device(source: &Device) -> io::Result<VirtualDevice> {
    let name = format!("{} (holdtap)", source.name().unwrap_or("Touchpad"));
    let mut builder = VirtualDevice::builder()
        .map_err(|error| context("open /dev/uinput", error))?
        .name(&name)
        .input_id(source.input_id());
    if let Some(keys) = source.supported_keys() {
        builder = builder
            .with_keys(keys)
            .map_err(|error| context("copy key capabilities", error))?;
    }
    if let Some(properties) = source.misc_properties() {
        builder = builder
            .with_msc(properties)
            .map_err(|error| context("copy MSC capabilities", error))?;
    }
    builder = builder
        .with_properties(source.properties())
        .map_err(|error| context("copy input properties", error))?;
    for (axis, info) in source.get_absinfo()? {
        builder = builder
            .with_absolute_axis(&UinputAbsSetup::new(axis, info))
            .map_err(|error| context(&format!("copy absolute axis 0x{:x}", axis.0), error))?;
    }
    builder
        .build()
        .map_err(|error| context("create virtual device", error))
}

fn context(operation: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{operation}: {error}"))
}

fn wait_readable(device: &Device, timeout_ms: i32) -> io::Result<()> {
    let mut descriptor = libc::pollfd {
        fd: device.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn restore_contact(
    output: &mut VirtualDevice,
    recognizer: &Recognizer,
    slot: u16,
) -> io::Result<()> {
    let Some((tracking_id, point)) = recognizer.contact(slot) else {
        return Ok(());
    };
    let mut events = vec![
        InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_SLOT.0,
            slot.into(),
        ),
        InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_TRACKING_ID.0,
            tracking_id,
        ),
    ];
    if let Some(point) = point {
        events.push(InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_POSITION_X.0,
            point.x,
        ));
        events.push(InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_POSITION_Y.0,
            point.y,
        ));
    }
    output.emit(&events)
}

fn emit_frame(
    output: &mut VirtualDevice,
    frame: &mut Vec<InputEvent>,
    recognizer: &Recognizer,
) -> io::Result<()> {
    for (code, value) in tool_count_events(recognizer.visible_contacts()) {
        frame.push(InputEvent::new(EventType::KEY.0, code, value));
    }
    if !frame.is_empty() {
        output.emit(frame)?;
        frame.clear();
    }
    Ok(())
}

fn is_tool_count(event: InputEvent) -> bool {
    event.event_type() == EventType::KEY
        && matches!(event.code(), 0x145 | 0x148 | 0x14d | 0x14e | 0x14f)
}

fn tool_count_events(count: usize) -> [(u16, i32); 5] {
    [
        (KeyCode::BTN_TOOL_FINGER.0, i32::from(count == 1)),
        (KeyCode::BTN_TOOL_DOUBLETAP.0, i32::from(count == 2)),
        (KeyCode::BTN_TOOL_TRIPLETAP.0, i32::from(count == 3)),
        (KeyCode::BTN_TOOL_QUADTAP.0, i32::from(count == 4)),
        (KeyCode::BTN_TOOL_QUINTTAP.0, i32::from(count >= 5)),
    ]
}
