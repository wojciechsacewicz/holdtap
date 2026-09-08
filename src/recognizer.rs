use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug)]
struct Contact {
    started: Duration,
    origin: Option<Point>,
    current: Option<Point>,
}

impl Contact {
    fn displacement_squared(self) -> i64 {
        let (Some(a), Some(b)) = (self.origin, self.current) else {
            return 0;
        };
        let dx = i64::from(b.x - a.x);
        let dy = i64::from(b.y - a.y);
        dx * dx + dy * dy
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Pass,
    Hide(u16),
    Click(u16),
    RestoreForScroll(u16),
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub min_primary_age: Duration,
    pub primary_move_mm: f64,
    pub tap_timeout: Duration,
    pub scroll_move_mm: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min_primary_age: Duration::from_millis(45),
            primary_move_mm: 1.5,
            tap_timeout: Duration::from_millis(140),
            scroll_move_mm: 0.8,
        }
    }
}

#[derive(Debug)]
pub struct Recognizer {
    contacts: BTreeMap<u16, Contact>,
    pending: Option<(u16, Duration)>,
    min_primary_age: Duration,
    primary_move_squared: i64,
    tap_timeout: Duration,
    scroll_move_squared: i64,
}

impl Recognizer {
    pub fn new(resolution: i32, config: Config) -> Self {
        let units = |mm: f64| (mm * f64::from(resolution.max(1))).round() as i64;
        Self {
            contacts: BTreeMap::new(),
            pending: None,
            min_primary_age: config.min_primary_age,
            primary_move_squared: units(config.primary_move_mm).pow(2),
            tap_timeout: config.tap_timeout,
            scroll_move_squared: units(config.scroll_move_mm).pow(2),
        }
    }

    pub fn touch_down(&mut self, slot: u16, _tracking_id: i32, now: Duration) -> Decision {
        let active_before = self.contacts.len();
        self.contacts.insert(
            slot,
            Contact {
                started: now,
                origin: None,
                current: None,
            },
        );

        if let Some((pending_slot, _)) = self.pending.take() {
            return Decision::RestoreForScroll(pending_slot);
        }

        if active_before == 1 {
            let primary = self
                .contacts
                .iter()
                .find(|(s, _)| **s != slot)
                .map(|(_, c)| *c);
            if let Some(primary) = primary
                && now.saturating_sub(primary.started) >= self.min_primary_age
                && primary.displacement_squared() >= self.primary_move_squared
            {
                self.pending = Some((slot, now));
                return Decision::Hide(slot);
            }
        }
        Decision::Pass
    }

    pub fn position(&mut self, slot: u16, point: Point, now: Duration) -> Decision {
        if let Some(contact) = self.contacts.get_mut(&slot) {
            contact.origin.get_or_insert(point);
            contact.current = Some(point);
        }
        self.classify(now)
    }

    pub fn tick(&mut self, now: Duration) -> Decision {
        self.classify(now)
    }

    fn classify(&mut self, now: Duration) -> Decision {
        let Some((slot, started)) = self.pending else {
            return Decision::Pass;
        };
        let Some(contact) = self.contacts.get(&slot) else {
            return Decision::Pass;
        };
        if contact.displacement_squared() >= self.scroll_move_squared
            || now.saturating_sub(started) >= self.tap_timeout
        {
            self.pending = None;
            return Decision::RestoreForScroll(slot);
        }
        Decision::Hide(slot)
    }

    pub fn touch_up(&mut self, slot: u16) -> Decision {
        self.contacts.remove(&slot);
        if self.pending.is_some_and(|(pending, _)| pending == slot) {
            self.pending = None;
            Decision::Click(slot)
        } else {
            self.pending.take().map_or(Decision::Pass, |(pending, _)| {
                Decision::RestoreForScroll(pending)
            })
        }
    }

    pub fn pending_slot(&self) -> Option<u16> {
        self.pending.map(|(slot, _)| slot)
    }

    pub fn visible_contacts(&self) -> usize {
        self.contacts.len() - usize::from(self.pending.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn established_primary(r: &mut Recognizer) {
        assert_eq!(r.touch_down(0, 10, ms(0)), Decision::Pass);
        r.position(0, Point { x: 100, y: 100 }, ms(1));
        r.position(0, Point { x: 125, y: 100 }, ms(60));
    }

    fn recognizer() -> Recognizer {
        Recognizer::new(12, Config::default())
    }

    #[test]
    fn quick_second_finger_tap_clicks() {
        let mut r = recognizer();
        established_primary(&mut r);
        assert_eq!(r.touch_down(1, 11, ms(70)), Decision::Hide(1));
        r.position(1, Point { x: 400, y: 400 }, ms(71));
        assert_eq!(r.touch_up(1), Decision::Click(1));
    }

    #[test]
    fn second_finger_motion_restores_scroll_early() {
        let mut r = recognizer();
        established_primary(&mut r);
        r.touch_down(1, 11, ms(70));
        r.position(1, Point { x: 400, y: 400 }, ms(71));
        assert_eq!(
            r.position(1, Point { x: 410, y: 400 }, ms(80)),
            Decision::RestoreForScroll(1)
        );
    }

    #[test]
    fn held_second_finger_restores_scroll_on_timeout() {
        let mut r = recognizer();
        established_primary(&mut r);
        r.touch_down(1, 11, ms(70));
        r.position(1, Point { x: 400, y: 400 }, ms(71));
        assert_eq!(r.tick(ms(209)), Decision::Hide(1));
        assert_eq!(r.tick(ms(210)), Decision::RestoreForScroll(1));
    }

    #[test]
    fn two_fingers_placed_together_pass_through() {
        let mut r = recognizer();
        r.touch_down(0, 10, ms(0));
        assert_eq!(r.touch_down(1, 11, ms(10)), Decision::Pass);
        assert_eq!(r.pending_slot(), None);
    }

    #[test]
    fn extra_fingers_are_not_intercepted() {
        let mut r = recognizer();
        established_primary(&mut r);
        r.touch_down(1, 11, ms(70));
        assert_eq!(r.touch_down(2, 12, ms(75)), Decision::RestoreForScroll(1));
        assert_eq!(r.visible_contacts(), 3);
    }
}
