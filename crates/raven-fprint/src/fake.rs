//! A sensor that scans nothing, and says so.
//!
//! The only [`Sensor`] this crate ships, for the two reasons `settings.rs`
//! gives for the fakes behind its Wi-Fi row: the rules above it can be built
//! and judged before any hardware exists, and the real one swaps in without
//! anything above it changing. The difference from a stub is that this one
//! never pretends — [`Fake::absent`] is what a machine with no reader gets,
//! and it answers [`Error::NoSensor`] to everything rather than quietly
//! succeeding.
//!
//! It is also how every rule in [`crate::enrol`] and [`crate::gate`] is
//! tested: a scripted run of readings, in microseconds, on a machine with no
//! fingerprint reader anywhere near it.

use crate::{Error, Finger, Scan, Sensor};

/// A sensor whose readings are whatever it was told to give.
#[derive(Debug, Clone, Default)]
pub struct Fake {
    /// `None` for a machine with no reader; see [`Fake::absent`].
    stages: Option<u8>,
    /// Readings still to be handed out, oldest first.
    script: Vec<Scan>,
    /// Good readings accumulated towards the finger being enrolled, as a real
    /// match-on-chip sensor accumulates them.
    pending: Option<(Finger, u8)>,
    enrolled: Vec<Finger>,
    abandons: u32,
}

impl Fake {
    /// A reader that wants `stages` good readings to enrol a finger.
    #[must_use]
    pub fn with_stages(stages: u8) -> Self {
        Self {
            stages: Some(stages),
            ..Self::default()
        }
    }

    /// No reader at all. Answers [`Error::NoSensor`] to everything.
    #[must_use]
    pub fn absent() -> Self {
        Self {
            stages: None,
            ..Self::default()
        }
    }

    /// Queue the readings this will hand out, oldest first.
    ///
    /// Running out is an error rather than a default reading: a test that
    /// takes one more reading than it meant to should say so loudly instead of
    /// quietly passing on a scan nobody wrote down.
    pub fn will(&mut self, scans: &[Scan]) {
        self.script = scans.to_vec();
    }

    /// Whether an enrolment was abandoned on this sensor.
    #[must_use]
    pub const fn abandoned(&self) -> bool {
        self.abandons > 0
    }

    /// How many times. Distinguishes "told once" from "told on every frame".
    #[must_use]
    pub const fn abandons(&self) -> u32 {
        self.abandons
    }

    /// Put a finger on it without going through an enrolment, for a test that
    /// is about verifying rather than about enrolling.
    pub fn pretend_enrolled(&mut self, finger: Finger) {
        if !self.enrolled.contains(&finger) {
            self.enrolled.push(finger);
        }
    }

    /// The next scripted reading.
    fn next(&mut self) -> Result<Scan, Error> {
        let Some(_) = self.stages else {
            return Err(Error::NoSensor);
        };
        if self.script.is_empty() {
            return Err(Error::Unreachable(
                "the fake sensor was asked for a reading it was never given".to_owned(),
            ));
        }
        Ok(self.script.remove(0))
    }
}

impl Sensor for Fake {
    fn stages(&self) -> u8 {
        self.stages.unwrap_or(0)
    }

    fn enrol_step(&mut self, finger: Finger) -> Result<Scan, Error> {
        let scan = self.next()?;
        let stages = self.stages.unwrap_or(0);
        if scan == Scan::Good {
            // Accumulate on the chip, as the hardware does: the count that
            // completes a template is the sensor's, and `Enrolment` keeping
            // its own tally of the same thing is what this proves agrees.
            let (_, done) = self.pending.get_or_insert((finger, 0));
            *done += 1;
            if *done >= stages {
                self.pending = None;
                if !self.enrolled.contains(&finger) {
                    self.enrolled.push(finger);
                }
            }
        }
        Ok(scan)
    }

    fn enrol_abandon(&mut self) {
        self.pending = None;
        self.abandons += 1;
    }

    fn verify(&mut self) -> Result<Scan, Error> {
        self.next()
    }

    fn enrolled(&self) -> Result<Vec<Finger>, Error> {
        if self.stages.is_none() {
            return Err(Error::NoSensor);
        }
        Ok(self.enrolled.clone())
    }

    fn forget(&mut self, finger: Finger) -> Result<(), Error> {
        if self.stages.is_none() {
            return Err(Error::NoSensor);
        }
        if !self.enrolled.contains(&finger) {
            return Err(Error::NotEnrolled(finger));
        }
        self.enrolled.retain(|f| *f != finger);
        Ok(())
    }

    fn forget_all(&mut self) -> Result<(), Error> {
        if self.stages.is_none() {
            return Err(Error::NoSensor);
        }
        self.enrolled.clear();
        self.pending = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine with no reader must not be able to enrol or verify anything,
    /// and must say which of those it is.
    #[test]
    fn an_absent_reader_refuses_everything() {
        let mut sensor = Fake::absent();
        assert_eq!(sensor.stages(), 0);
        assert_eq!(sensor.verify(), Err(Error::NoSensor));
        assert_eq!(sensor.enrol_step(Finger::RightIndex), Err(Error::NoSensor));
        assert_eq!(sensor.enrolled(), Err(Error::NoSensor));
        assert_eq!(sensor.forget_all(), Err(Error::NoSensor));
    }

    #[test]
    fn a_reading_nobody_scripted_is_an_error_and_not_a_guess() {
        let mut sensor = Fake::with_stages(2);
        assert!(matches!(sensor.verify(), Err(Error::Unreachable(_))));
    }

    #[test]
    fn forgetting_a_finger_that_is_not_there_says_which() {
        let mut sensor = Fake::with_stages(2);
        assert_eq!(
            sensor.forget(Finger::LeftRing),
            Err(Error::NotEnrolled(Finger::LeftRing))
        );
    }

    #[test]
    fn a_finger_appears_once_the_sensor_has_its_stages() {
        let mut sensor = Fake::with_stages(2);
        sensor.will(&[Scan::Good, Scan::Good]);
        sensor.enrol_step(Finger::LeftThumb).unwrap();
        assert!(
            sensor.enrolled().unwrap().is_empty(),
            "half a template is not a finger"
        );
        sensor.enrol_step(Finger::LeftThumb).unwrap();
        assert_eq!(sensor.enrolled().unwrap(), vec![Finger::LeftThumb]);
    }

    #[test]
    fn forgetting_works() {
        let mut sensor = Fake::with_stages(1);
        sensor.pretend_enrolled(Finger::RightIndex);
        sensor.forget(Finger::RightIndex).unwrap();
        assert!(sensor.enrolled().unwrap().is_empty());
    }
}
