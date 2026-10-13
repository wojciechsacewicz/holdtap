#![cfg(target_os = "linux")]

use evdev::{
    AbsInfo, AbsoluteAxisCode as Axis, AttributeSet, Device, EventType, InputEvent, KeyCode,
    UinputAbsSetup, uinput::VirtualDevice,
};
use std::{
    io,
    os::fd::AsRawFd,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

struct RunningHoldtap(Child);

impl Drop for RunningHoldtap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn abs(axis: Axis, value: i32) -> InputEvent {
    InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
}

fn key(key: KeyCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::KEY.0, key.0, value)
}

fn contact(slot: i32, id: i32, x: i32) -> [InputEvent; 4] {
    [
        abs(Axis::ABS_MT_SLOT, slot),
        abs(Axis::ABS_MT_TRACKING_ID, id),
        abs(Axis::ABS_MT_POSITION_X, x),
        abs(Axis::ABS_MT_POSITION_Y, 300),
    ]
}

fn eventually<T>(mut read: impl FnMut() -> Option<T>, message: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(value) = read() {
            return value;
        }
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn kernel_contacts(device: &Device) -> io::Result<([i32; 2], bool)> {
    let mut slots = [Axis::ABS_MT_TRACKING_ID.0 as i32, -1, -1];
    let request = (2_u64 << 30)
        | ((std::mem::size_of_val(&slots) as u64) << 16)
        | (u64::from(b'E') << 8)
        | 0x0a;
    if unsafe {
        libc::ioctl(
            device.as_raw_fd(),
            request as libc::c_ulong,
            slots.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let keys = device.get_key_state()?;
    Ok((
        [slots[1], slots[2]],
        keys.contains(KeyCode::BTN_TOOL_DOUBLETAP),
    ))
}

fn assert_contacts(device: &Device, expected: [i32; 2], double: bool, stage: &str) {
    let mut actual = kernel_contacts(device).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while actual != (expected, double) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
        actual = kernel_contacts(device).unwrap();
    }
    assert_eq!(
        actual,
        (expected, double),
        "{stage}: kernel tracking IDs and two-finger tool key"
    );
}

#[test]
#[ignore = "requires /dev/uinput and evdev access; uses synthetic devices only"]
fn recovers_multitouch_slots_after_kernel_queue_overflow() -> io::Result<()> {
    let name = format!("holdtap queue regression {}", std::process::id());
    let mut keys = AttributeSet::new();
    for key in [
        KeyCode::BTN_LEFT,
        KeyCode::BTN_TOUCH,
        KeyCode::BTN_TOOL_FINGER,
        KeyCode::BTN_TOOL_DOUBLETAP,
        KeyCode::BTN_TOOL_TRIPLETAP,
        KeyCode::BTN_TOOL_QUADTAP,
        KeyCode::BTN_TOOL_QUINTTAP,
    ] {
        keys.insert(key);
    }
    // No POINTER property: neither synthetic device participates in desktop gestures.
    let mut builder = VirtualDevice::builder()?.name(&name).with_keys(&keys)?;
    for (axis, max) in [
        (Axis::ABS_MT_SLOT, 1),
        (Axis::ABS_MT_TRACKING_ID, 65535),
        (Axis::ABS_MT_POSITION_X, 10000),
        (Axis::ABS_MT_POSITION_Y, 10000),
    ] {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(
            axis,
            AbsInfo::new(0, 0, max, 0, 0, 12),
        ))?;
    }
    let mut source = builder.build()?;
    let path = source
        .enumerate_dev_nodes_blocking()?
        .next()
        .expect("synthetic evdev node")?;
    let mut overflow_observer = eventually(
        || evdev::raw_stream::RawDevice::open(&path).ok(),
        "synthetic source node did not become accessible",
    );
    assert_eq!(
        unsafe {
            libc::fcntl(
                overflow_observer.as_raw_fd(),
                libc::F_SETFL,
                libc::O_NONBLOCK,
            )
        },
        0
    );
    for frame in 0..10000 {
        source.emit(&[abs(Axis::ABS_MT_POSITION_X, 100 + frame % 5000)])?;
    }
    assert!(
        overflow_observer.fetch_events()?.any(|event| {
            event.event_type() == EventType::SYNCHRONIZATION
                && event.code() == evdev::SynchronizationCode::SYN_DROPPED.0
        }),
        "synthetic source queue did not overflow during calibration"
    );
    eprintln!("calibration confirmed SYN_DROPPED from 10000 motion frames");
    drop(overflow_observer);
    source.emit(&[abs(Axis::ABS_MT_POSITION_X, 100)])?;
    let mut child = RunningHoldtap(
        Command::new(env!("CARGO_BIN_EXE_holdtap"))
            .arg("--device")
            .arg(path)
            .spawn()?,
    );
    let output_name = format!("{name} (holdtap)");
    let output = eventually(
        || {
            evdev::enumerate().find_map(|(_, device)| {
                (device.name() == Some(output_name.as_str())).then_some(device)
            })
        },
        "HoldTap output device did not appear",
    );
    thread::sleep(Duration::from_millis(100));
    source.emit(&contact(0, 10, 100))?;
    source.emit(&[key(KeyCode::BTN_TOUCH, 1), key(KeyCode::BTN_TOOL_FINGER, 1)])?;
    assert_contacts(&output, [10, -1], false, "initial contact");

    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGSTOP) }, 0);
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(child.0.id() as i32, &mut status, libc::WUNTRACED) },
        child.0.id() as i32
    );
    assert!(libc::WIFSTOPPED(status));
    source.emit(&contact(1, 11, 500))?;
    source.emit(&[
        key(KeyCode::BTN_TOOL_FINGER, 0),
        key(KeyCode::BTN_TOOL_DOUBLETAP, 1),
        abs(Axis::ABS_MT_SLOT, 0),
    ])?;
    for frame in 0..10000 {
        source.emit(&[abs(Axis::ABS_MT_POSITION_X, 100 + frame % 5000)])?;
    }
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGCONT) }, 0);
    assert_contacts(&output, [10, 11], true, "after dropped-event recovery");

    source.emit(&[
        abs(Axis::ABS_MT_SLOT, 0),
        abs(Axis::ABS_MT_TRACKING_ID, -1),
        abs(Axis::ABS_MT_SLOT, 1),
        abs(Axis::ABS_MT_TRACKING_ID, -1),
        key(KeyCode::BTN_TOUCH, 0),
        key(KeyCode::BTN_TOOL_DOUBLETAP, 0),
    ])?;
    assert_contacts(&output, [-1, -1], false, "all fingers lifted");
    source.emit(&contact(0, 20, 100))?;
    source.emit(&[key(KeyCode::BTN_TOUCH, 1), key(KeyCode::BTN_TOOL_FINGER, 1)])?;
    source.emit(&contact(1, 21, 500))?;
    source.emit(&[
        key(KeyCode::BTN_TOOL_FINGER, 0),
        key(KeyCode::BTN_TOOL_DOUBLETAP, 1),
    ])?;
    assert_contacts(&output, [20, 21], true, "next two-finger gesture");
    assert!(child.0.try_wait()?.is_none(), "HoldTap exited unexpectedly");
    Ok(())
}
