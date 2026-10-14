mod recognizer;

use evdev::raw_stream::RawDevice;
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, PropType, SynchronizationCode,
    UinputAbsSetup,
};
use recognizer::{Config, Decision, Point, Recognizer};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const POLL_MS: i32 = 10;
const MAX_RECOVERY_BATCHES: usize = 32;

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

#[derive(Default)]
struct SlotState {
    axes: BTreeMap<u16, i32>,
}

struct SlotLayout {
    count: u16,
    axes: Vec<AbsoluteAxisCode>,
    keys: Vec<KeyCode>,
}

struct SourceSnapshot {
    current_slot: u16,
    slots: BTreeMap<u16, SlotState>,
    keys: AttributeSet<KeyCode>,
    other_axes: Vec<(AbsoluteAxisCode, i32)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    Forwarding,
    DiscardUntilReport,
    SnapshotPending,
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

fn find_device(requested: Option<&str>) -> Option<(PathBuf, RawDevice)> {
    evdev::raw_stream::enumerate().find(|(path, device)| {
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

fn is_touchpad(device: &RawDevice) -> bool {
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

fn run_device(mut source: RawDevice, config: Config) -> io::Result<()> {
    let mut output = clone_device(&source)?;
    let layout = slot_layout(&source)?;
    let resolution = source
        .get_absinfo()?
        .find(|(axis, _)| *axis == AbsoluteAxisCode::ABS_MT_POSITION_X)
        .map_or(12, |(_, info)| info.resolution());
    let mut recognizer = Recognizer::new(resolution, config);
    let mut slots = BTreeMap::<u16, SlotState>::new();
    let mut current_slot = 0_u16;
    let mut recovery = Recovery::SnapshotPending;
    let mut dropped = false;
    let started = Instant::now();

    source.grab()?;
    set_nonblocking(&source)?;
    eprintln!("holdtap: active (resolution {resolution} units/mm)");

    let mut frame = Vec::new();
    loop {
        wait_readable(&source, POLL_MS)?;
        let now = started.elapsed();
        let mut events = Vec::new();
        match source.fetch_events() {
            Ok(batch) => events.extend(batch),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }

        if let Some(index) = events.iter().position(|event| {
            event.event_type() == EventType::SYNCHRONIZATION
                && event.code() == SynchronizationCode::SYN_DROPPED.0
        }) {
            frame.clear();
            recognizer.reset();
            release_output(&mut output, &layout, &recognizer)?;
            recovery = Recovery::DiscardUntilReport;
            discard_events(&events[index..], &mut recovery);
            events.clear();
            if !dropped {
                eprintln!("holdtap: input events dropped; resynchronizing multitouch state");
            }
            dropped = true;
        }

        if recovery != Recovery::Forwarding {
            discard_events(&events, &mut recovery);
            if !drain_source(&mut source, &mut recovery)? {
                continue;
            }
            let snapshot = snapshot_source(&source, &layout)?;
            // Separate axis ioctls can span a physical frame. Retry if input arrived.
            match source.fetch_events() {
                Ok(batch) => {
                    let tail: Vec<_> = batch.collect();
                    if !tail.is_empty() {
                        discard_events(&tail, &mut recovery);
                        continue;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            frame.clear();
            recognizer.reset();
            release_output(&mut output, &layout, &recognizer)?;
            install_snapshot(
                &mut frame,
                &mut recognizer,
                &mut slots,
                &mut current_slot,
                snapshot,
                now,
            );
            emit_frame(&mut output, &mut frame, &recognizer)?;
            recovery = Recovery::Forwarding;
            if dropped {
                eprintln!(
                    "holdtap: restored {} contact(s) after input loss",
                    recognizer.contact_count()
                );
                dropped = false;
            }
            continue;
        }

        if events.is_empty() {
            if let Decision::RestoreForScroll(slot) = recognizer.tick(now) {
                restore_contact(&mut frame, &slots, slot);
                emit_frame(&mut output, &mut frame, &recognizer)?;
            }
            continue;
        }

        for event in events {
            if event.event_type() == EventType::SYNCHRONIZATION && event.code() == 0 {
                // Evaluate complete coordinates, never a new X paired with an old Y.
                for (&slot, state) in &slots {
                    if let (Some(&x), Some(&y)) = (
                        state.axes.get(&AbsoluteAxisCode::ABS_MT_POSITION_X.0),
                        state.axes.get(&AbsoluteAxisCode::ABS_MT_POSITION_Y.0),
                    ) && let Decision::RestoreForScroll(pending) =
                        recognizer.position(slot, Point { x, y }, now)
                    {
                        restore_contact(&mut frame, &slots, pending);
                    }
                }
                emit_frame(&mut output, &mut frame, &recognizer)?;
                continue;
            }

            let mut decision = Decision::Pass;
            if event.event_type() == EventType::ABSOLUTE {
                if is_mt_attribute(event.code()) {
                    slots
                        .entry(current_slot)
                        .or_default()
                        .axes
                        .insert(event.code(), event.value());
                }
                match event.code() {
                    code if code == AbsoluteAxisCode::ABS_MT_SLOT.0 => {
                        current_slot = event.value() as u16;
                    }
                    code if code == AbsoluteAxisCode::ABS_MT_TRACKING_ID.0 => {
                        if event.value() >= 0 {
                            decision = recognizer.touch_down(current_slot, event.value(), now);
                        } else {
                            decision = recognizer.touch_up(current_slot);
                        }
                    }
                    _ => {}
                }
            }

            if event.event_type() == EventType::KEY
                && event.code() == KeyCode::BTN_TOUCH.0
                && event.value() == 0
            {
                reset_after_all_fingers_lifted(&mut frame, &mut slots, &mut recognizer);
            }

            if let Decision::RestoreForScroll(slot) = decision {
                restore_contact(&mut frame, &slots, slot);
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
                if event.event_type() == EventType::ABSOLUTE
                    && event.code() == AbsoluteAxisCode::ABS_MT_TRACKING_ID.0
                    && event.value() >= 0
                {
                    restore_contact(&mut frame, &slots, current_slot);
                } else {
                    queue_event(&mut frame, current_slot, event);
                }
            }

            if matches!(decision, Decision::Click(_)) {
                emit_frame(&mut output, &mut frame, &recognizer)?;
                output.emit(&[InputEvent::new(EventType::KEY.0, KeyCode::BTN_LEFT.0, 1)])?;
                output.emit(&[InputEvent::new(EventType::KEY.0, KeyCode::BTN_LEFT.0, 0)])?;
            }
        }
    }
}

fn clone_device(source: &RawDevice) -> io::Result<VirtualDevice> {
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

fn set_nonblocking(source: &RawDevice) -> io::Result<()> {
    let fd = source.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn slot_layout(source: &RawDevice) -> io::Result<SlotLayout> {
    let info = source
        .get_absinfo()?
        .find(|(axis, _)| *axis == AbsoluteAxisCode::ABS_MT_SLOT)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing multitouch slots"))?
        .1;
    // EVIOCGMTSLOTS encodes its buffer length in a 14-bit ioctl size field.
    if info.minimum() != 0 || !(0..4094).contains(&info.maximum()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid multitouch slot range",
        ));
    }
    Ok(SlotLayout {
        count: (info.maximum() + 1) as u16,
        axes: source
            .supported_absolute_axes()
            .into_iter()
            .flat_map(|axes| axes.iter())
            .filter(|axis| is_mt_attribute(axis.0))
            .collect(),
        keys: source
            .supported_keys()
            .into_iter()
            .flat_map(|keys| keys.iter())
            .collect(),
    })
}

fn snapshot_source(source: &RawDevice, layout: &SlotLayout) -> io::Result<SourceSnapshot> {
    let absolute = source.get_abs_state()?;
    let current_slot = absolute[usize::from(AbsoluteAxisCode::ABS_MT_SLOT.0)].value;
    if !(0..i32::from(layout.count)).contains(&current_slot) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid selected multitouch slot",
        ));
    }
    let mut slots: BTreeMap<_, _> = (0..layout.count)
        .map(|slot| (slot, SlotState::default()))
        .collect();
    for axis in &layout.axes {
        let mut values = vec![0_i32; usize::from(layout.count) + 1];
        values[0] = i32::from(axis.0);
        let additional_bytes = usize::from(layout.count) * std::mem::size_of::<i32>();
        let request =
            libc::_IOR::<i32>(u32::from(b'E'), 0x0a) + ((additional_bytes as libc::c_ulong) << 16);
        // The first i32 is the axis selector, followed by one i32 per kernel slot.
        if unsafe { libc::ioctl(source.as_raw_fd(), request, values.as_mut_ptr()) } < 0 {
            return Err(context(
                &format!("read multitouch axis 0x{:x}", axis.0),
                io::Error::last_os_error(),
            ));
        }
        for (state, value) in slots.values_mut().zip(values.into_iter().skip(1)) {
            state.axes.insert(axis.0, value);
        }
    }
    Ok(SourceSnapshot {
        current_slot: current_slot as u16,
        slots,
        keys: source.get_key_state()?,
        other_axes: source
            .supported_absolute_axes()
            .into_iter()
            .flat_map(|axes| axes.iter())
            .filter(|axis| *axis != AbsoluteAxisCode::ABS_MT_SLOT && !is_mt_attribute(axis.0))
            .map(|axis| (axis, absolute[usize::from(axis.0)].value))
            .collect(),
    })
}

fn discard_events(events: &[InputEvent], recovery: &mut Recovery) {
    for event in events {
        *recovery = if event.event_type() == EventType::SYNCHRONIZATION
            && event.code() == SynchronizationCode::SYN_REPORT.0
        {
            Recovery::SnapshotPending
        } else {
            Recovery::DiscardUntilReport
        };
    }
}

fn drain_source(source: &mut RawDevice, recovery: &mut Recovery) -> io::Result<bool> {
    for _ in 0..MAX_RECOVERY_BATCHES {
        match source.fetch_events() {
            Ok(batch) => {
                let events: Vec<_> = batch.collect();
                if events.is_empty() {
                    return Ok(*recovery == Recovery::SnapshotPending);
                }
                discard_events(&events, recovery);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(*recovery == Recovery::SnapshotPending);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn release_output(
    output: &mut VirtualDevice,
    layout: &SlotLayout,
    recognizer: &Recognizer,
) -> io::Result<()> {
    let mut frame = vec![InputEvent::new(EventType::KEY.0, KeyCode::BTN_TOUCH.0, 0)];
    for slot in 0..layout.count {
        queue_event(
            &mut frame,
            slot,
            InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_MT_TRACKING_ID.0,
                -1,
            ),
        );
    }
    for key in &layout.keys {
        if *key != KeyCode::BTN_TOUCH {
            frame.push(InputEvent::new(EventType::KEY.0, key.0, 0));
        }
    }
    emit_frame(output, &mut frame, recognizer)
}

fn install_snapshot(
    frame: &mut Vec<InputEvent>,
    recognizer: &mut Recognizer,
    slots: &mut BTreeMap<u16, SlotState>,
    current_slot: &mut u16,
    snapshot: SourceSnapshot,
    now: Duration,
) {
    *current_slot = snapshot.current_slot;
    *slots = snapshot.slots;
    recognizer.resynchronize(
        slots.iter().filter_map(|(&slot, state)| {
            let tracking_id = *state.axes.get(&AbsoluteAxisCode::ABS_MT_TRACKING_ID.0)?;
            if tracking_id < 0 {
                return None;
            }
            Some((
                slot,
                Point {
                    x: *state.axes.get(&AbsoluteAxisCode::ABS_MT_POSITION_X.0)?,
                    y: *state.axes.get(&AbsoluteAxisCode::ABS_MT_POSITION_Y.0)?,
                },
            ))
        }),
        now,
    );
    frame.push(InputEvent::new(
        EventType::KEY.0,
        KeyCode::BTN_TOUCH.0,
        i32::from(snapshot.keys.contains(KeyCode::BTN_TOUCH)),
    ));
    for (&slot, state) in slots.iter() {
        if let Some(&tracking_id) = state.axes.get(&AbsoluteAxisCode::ABS_MT_TRACKING_ID.0) {
            queue_event(
                frame,
                slot,
                InputEvent::new(
                    EventType::ABSOLUTE.0,
                    AbsoluteAxisCode::ABS_MT_TRACKING_ID.0,
                    tracking_id,
                ),
            );
        }
        for (&code, &value) in &state.axes {
            if code != AbsoluteAxisCode::ABS_MT_TRACKING_ID.0 {
                queue_event(
                    frame,
                    slot,
                    InputEvent::new(EventType::ABSOLUTE.0, code, value),
                );
            }
        }
    }
    for (axis, value) in snapshot.other_axes {
        frame.push(InputEvent::new(EventType::ABSOLUTE.0, axis.0, value));
    }
    for key in snapshot.keys.iter() {
        let event = InputEvent::new(EventType::KEY.0, key.0, 1);
        if key != KeyCode::BTN_TOUCH && !is_tool_count(event) {
            frame.push(event);
        }
    }
}

fn wait_readable(device: &RawDevice, timeout_ms: i32) -> io::Result<()> {
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

// Type-B slots retain axis values across frames and contact lifetimes.
fn is_mt_attribute(code: u16) -> bool {
    (AbsoluteAxisCode::ABS_MT_TOUCH_MAJOR.0..=AbsoluteAxisCode::ABS_MT_TOOL_Y.0).contains(&code)
}

fn queue_event(frame: &mut Vec<InputEvent>, slot: u16, event: InputEvent) {
    if event.event_type() == EventType::ABSOLUTE && is_mt_attribute(event.code()) {
        // Filtering and synthetic restoration can change the output slot independently
        // of the source. Select it explicitly before every forwarded MT attribute.
        frame.push(InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_SLOT.0,
            slot.into(),
        ));
    }
    frame.push(event);
}

fn restore_contact(frame: &mut Vec<InputEvent>, slots: &BTreeMap<u16, SlotState>, slot: u16) {
    let Some(state) = slots.get(&slot) else {
        return;
    };
    let Some(&tracking_id) = state.axes.get(&AbsoluteAxisCode::ABS_MT_TRACKING_ID.0) else {
        return;
    };
    if tracking_id < 0 {
        return;
    }
    queue_event(
        frame,
        slot,
        InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_MT_TRACKING_ID.0,
            tracking_id,
        ),
    );
    for (&code, &value) in &state.axes {
        if code != AbsoluteAxisCode::ABS_MT_TRACKING_ID.0 {
            queue_event(
                frame,
                slot,
                InputEvent::new(EventType::ABSOLUTE.0, code, value),
            );
        }
    }
}

fn reset_after_all_fingers_lifted(
    frame: &mut Vec<InputEvent>,
    slots: &mut BTreeMap<u16, SlotState>,
    recognizer: &mut Recognizer,
) {
    let active_slots = slots
        .values()
        .filter(|state| {
            state
                .axes
                .get(&AbsoluteAxisCode::ABS_MT_TRACKING_ID.0)
                .is_some_and(|tracking_id| *tracking_id >= 0)
        })
        .count();
    let stale_contacts = recognizer.contact_count().max(active_slots);

    for (&slot, state) in slots.iter_mut() {
        queue_event(
            frame,
            slot,
            InputEvent::new(
                EventType::ABSOLUTE.0,
                AbsoluteAxisCode::ABS_MT_TRACKING_ID.0,
                -1,
            ),
        );
        state
            .axes
            .insert(AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, -1);
    }

    recognizer.reset();

    if stale_contacts > 0 {
        eprintln!(
            "holdtap: resynchronized {stale_contacts} stale contact(s) after all fingers lifted"
        );
    }
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

#[cfg(test)]
mod stream_tests {
    use super::*;

    fn abs(axis: AbsoluteAxisCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
    }

    fn contacts() -> BTreeMap<u16, SlotState> {
        BTreeMap::from([(
            1,
            SlotState {
                axes: BTreeMap::from([
                    (AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, 11),
                    (AbsoluteAxisCode::ABS_MT_POSITION_X.0, 400),
                    (AbsoluteAxisCode::ABS_MT_POSITION_Y.0, 500),
                    (AbsoluteAxisCode::ABS_MT_PRESSURE.0, 30),
                    (AbsoluteAxisCode::ABS_MT_TOUCH_MAJOR.0, 8),
                ]),
            },
        )])
    }

    fn replay(events: &[InputEvent]) -> Vec<(i32, u16, i32)> {
        let mut slot = 0;
        let mut result = Vec::new();
        for event in events {
            if event.event_type() == EventType::ABSOLUTE {
                if event.code() == AbsoluteAxisCode::ABS_MT_SLOT.0 {
                    slot = event.value();
                } else if is_mt_attribute(event.code()) {
                    result.push((slot, event.code(), event.value()));
                }
            }
        }
        result
    }

    #[test]
    fn timeout_restore_does_not_redirect_primary_motion_or_release() {
        let mut frame = Vec::new();
        restore_contact(&mut frame, &contacts(), 1);
        // The physical device still has slot 0 selected and omits ABS_MT_SLOT.
        queue_event(&mut frame, 0, abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 130));
        queue_event(&mut frame, 0, abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1));
        let events = replay(&frame);
        assert_eq!(
            &events[events.len() - 2..],
            &[
                (0, AbsoluteAxisCode::ABS_MT_POSITION_X.0, 130),
                (0, AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, -1),
            ]
        );
    }

    #[test]
    fn restore_preserves_buffered_order_and_third_finger_slot() {
        let mut frame = Vec::new();
        queue_event(&mut frame, 0, abs(AbsoluteAxisCode::ABS_MT_POSITION_X, 130));
        restore_contact(&mut frame, &contacts(), 1);
        queue_event(&mut frame, 2, abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 12));
        let events = replay(&frame);
        assert_eq!(
            events.first(),
            Some(&(0, AbsoluteAxisCode::ABS_MT_POSITION_X.0, 130))
        );
        assert_eq!(
            events.last(),
            Some(&(2, AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, 12))
        );
        assert!(events.contains(&(1, AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, 11)));
    }

    #[test]
    fn restore_includes_cached_contact_shape_and_pressure() {
        let mut frame = Vec::new();
        restore_contact(&mut frame, &contacts(), 1);
        let events = replay(&frame);
        assert!(events.contains(&(1, AbsoluteAxisCode::ABS_MT_PRESSURE.0, 30)));
        assert!(events.contains(&(1, AbsoluteAxisCode::ABS_MT_TOUCH_MAJOR.0, 8)));
        assert!(events.contains(&(1, AbsoluteAxisCode::ABS_MT_POSITION_Y.0, 500)));
    }

    #[test]
    fn all_fingers_up_preserves_axes_for_next_hidden_contact() {
        let mut frame = Vec::new();
        let mut slots = contacts();
        let mut recognizer = Recognizer::new(12, Config::default());
        recognizer.touch_down(1, 11, Duration::ZERO);

        reset_after_all_fingers_lifted(&mut frame, &mut slots, &mut recognizer);
        frame.clear();

        let state = slots.entry(1).or_default();
        state
            .axes
            .insert(AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, 12);
        state
            .axes
            .insert(AbsoluteAxisCode::ABS_MT_POSITION_X.0, 420);
        restore_contact(&mut frame, &slots, 1);

        let events = replay(&frame);
        assert!(events.contains(&(1, AbsoluteAxisCode::ABS_MT_POSITION_Y.0, 500)));
        assert!(events.contains(&(1, AbsoluteAxisCode::ABS_MT_PRESSURE.0, 30)));
        assert!(events.contains(&(1, AbsoluteAxisCode::ABS_MT_TOUCH_MAJOR.0, 8)));
    }

    #[test]
    fn all_fingers_up_releases_stale_slots_and_resets_recognizer() {
        let mut frame = Vec::new();
        let mut slots = contacts();
        slots.insert(
            2,
            SlotState {
                axes: BTreeMap::from([(AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, -1)]),
            },
        );
        let mut recognizer = Recognizer::new(12, Config::default());
        recognizer.touch_down(0, 10, Duration::ZERO);
        recognizer.touch_down(1, 11, Duration::from_millis(10));

        reset_after_all_fingers_lifted(&mut frame, &mut slots, &mut recognizer);

        assert!(replay(&frame).contains(&(1, AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, -1)));
        assert!(replay(&frame).contains(&(2, AbsoluteAxisCode::ABS_MT_TRACKING_ID.0, -1)));
        assert_eq!(slots[&1].axes[&AbsoluteAxisCode::ABS_MT_TRACKING_ID.0], -1);
        assert_eq!(slots[&1].axes[&AbsoluteAxisCode::ABS_MT_POSITION_Y.0], 500);
        assert_eq!(recognizer.contact_count(), 0);
        assert_eq!(recognizer.pending_slot(), None);
    }
}
