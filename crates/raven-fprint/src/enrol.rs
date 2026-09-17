//! Teaching a sensor a finger: how many presentations, and when to give up.
//!
//! Enrolment is a loop with two ways out, and the interesting one is the way
//! out that is not success. A reader that will not produce a usable reading —
//! a dry finger in winter, a smear of something on the glass, a sensor that has
//! decided today is not its day — will go on not producing one for as long as
//! anybody keeps pressing, and a dialog with no floor under it is a dialog that
//! asks for a fingertip forever.
//!
//! So the count of readings that worked and the count of readings that did not
//! are kept apart, and only the first is progress. [`PATIENCE`] is the floor.
//!
//! # Why the sensor counts the stages and this counts the tries
//!
//! How many good readings make a template is a property of the hardware and
//! varies from about five to about twenty; [`Sensor::stages`] is the sensor's
//! to answer and a number invented here would be a progress bar that lies. How
//! many bad readings are worth sitting through is a property of the person's
//! patience, which no sensor knows anything about.

use crate::{Error, Finger, Scan, Sensor};

/// How many unusable readings in a row end an enrolment.
///
/// Generous: the first few readings of a new finger are routinely poor while
/// somebody works out how hard to press and where the sensor actually is, and
/// an enrolment that gave up after three would be one that mostly gives up.
/// Low enough that a reader with something wrong with it says so within a few
/// seconds rather than after a minute of being pressed.
///
/// In a row, not in total, and this is the whole reason it works: a finger
/// that is producing good readings with the occasional bad one is a finger
/// being enrolled, however many bad ones it accumulates on the way.
pub const PATIENCE: u8 = 10;

/// How far along an enrolment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Another reading is wanted. `done` of `stages` are in, so a dialog can
    /// draw the pips it has filled and the ones it has not.
    More { done: u8, stages: u8 },
    /// That reading was no good; the one before it still stands. Say why and
    /// ask again — `patience` more of these and it will stop.
    Again {
        why: crate::Retry,
        done: u8,
        stages: u8,
        patience: u8,
    },
    /// The finger is enrolled.
    Enrolled(Finger),
    /// [`PATIENCE`] unusable readings in a row. The sensor has been told to
    /// forget what it had accumulated.
    GaveUp,
}

/// One finger being enrolled, from the first presentation to the last.
///
/// Holds no sensor: it is handed one per reading, so that the same enrolment
/// can be driven from a thread that owns the hardware while the dialog that is
/// drawing it owns this. That separation is also what lets every rule below be
/// tested against [`crate::fake::Fake`] in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enrolment {
    finger: Finger,
    stages: u8,
    done: u8,
    /// Unusable readings since the last good one. Reset by a good one, which
    /// is what makes [`PATIENCE`] a run rather than a total.
    wasted: u8,
    finished: bool,
}

impl Enrolment {
    /// Begin enrolling `finger` on `sensor`.
    ///
    /// Asks the sensor how many stages it wants and refuses to begin if the
    /// answer is zero, which would otherwise be an enrolment that reports
    /// itself complete before anybody has touched anything.
    pub fn begin<S: Sensor + ?Sized>(sensor: &S, finger: Finger) -> Result<Self, Error> {
        let stages = sensor.stages();
        if stages == 0 {
            return Err(Error::Unreachable(
                "the reader wants no readings at all to enrol a finger".to_owned(),
            ));
        }
        Ok(Self {
            finger,
            stages,
            done: 0,
            wasted: 0,
            finished: false,
        })
    }

    /// Which finger this is for.
    #[must_use]
    pub const fn finger(&self) -> Finger {
        self.finger
    }

    /// How many good readings the sensor wants in all.
    #[must_use]
    pub const fn stages(&self) -> u8 {
        self.stages
    }

    /// How many it has.
    #[must_use]
    pub const fn done(&self) -> u8 {
        self.done
    }

    /// Whether this enrolment is over, either way.
    #[must_use]
    pub const fn finished(&self) -> bool {
        self.finished
    }

    /// Take one reading.
    ///
    /// Blocks in the sensor for as long as it takes somebody to present a
    /// finger, so this is called from a thread that is allowed to wait.
    ///
    /// Returns [`Progress::Enrolled`] or [`Progress::GaveUp`] exactly once;
    /// calling again after either is a programming error rather than a
    /// security one, and is answered with the same verdict rather than a
    /// panic — a dialog racing its own reader thread should not take the
    /// desktop down with it.
    pub fn step<S: Sensor + ?Sized>(&mut self, sensor: &mut S) -> Result<Progress, Error> {
        if self.finished {
            return Ok(if self.done >= self.stages {
                Progress::Enrolled(self.finger)
            } else {
                Progress::GaveUp
            });
        }
        // `NoMatch` has no meaning while enrolling: there is nothing yet to
        // match against. A sensor that says it anyway is answering a question
        // nobody asked, and the safe reading is that the sample was no good.
        let scan = match sensor.enrol_step(self.finger)? {
            Scan::NoMatch => Scan::Retry(crate::Retry::Unreadable),
            other => other,
        };
        match scan {
            Scan::Good => {
                self.wasted = 0;
                self.done = self.done.saturating_add(1);
                if self.done >= self.stages {
                    self.finished = true;
                    Ok(Progress::Enrolled(self.finger))
                } else {
                    Ok(Progress::More {
                        done: self.done,
                        stages: self.stages,
                    })
                }
            }
            Scan::Retry(why) => {
                self.wasted = self.wasted.saturating_add(1);
                if self.wasted >= PATIENCE {
                    self.finished = true;
                    // The sensor is holding a part-built template for a finger
                    // that is not going to be enrolled. Left there it is a
                    // slot gone from a reader that has about five.
                    sensor.enrol_abandon();
                    Ok(Progress::GaveUp)
                } else {
                    Ok(Progress::Again {
                        why,
                        done: self.done,
                        stages: self.stages,
                        patience: PATIENCE - self.wasted,
                    })
                }
            }
            Scan::NoMatch => unreachable!("mapped to Retry above"),
        }
    }

    /// Stop, because somebody closed the dialog or the screen locked.
    ///
    /// Tells the sensor to discard what it has. Safe to call twice.
    pub fn abandon<S: Sensor + ?Sized>(&mut self, sensor: &mut S) {
        if !self.finished {
            self.finished = true;
            sensor.enrol_abandon();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Retry;
    use crate::fake::Fake;

    #[test]
    fn enough_good_readings_enrol_a_finger() {
        let mut sensor = Fake::with_stages(4);
        let mut enrolment = Enrolment::begin(&sensor, Finger::RightIndex).unwrap();
        sensor.will(&[Scan::Good, Scan::Good, Scan::Good, Scan::Good]);

        assert_eq!(
            enrolment.step(&mut sensor).unwrap(),
            Progress::More {
                done: 1,
                stages: 4
            }
        );
        enrolment.step(&mut sensor).unwrap();
        enrolment.step(&mut sensor).unwrap();
        assert_eq!(
            enrolment.step(&mut sensor).unwrap(),
            Progress::Enrolled(Finger::RightIndex)
        );
        assert!(enrolment.finished());
        assert_eq!(sensor.enrolled().unwrap(), vec![Finger::RightIndex]);
    }

    /// The point of the whole module: a bad reading does not go backwards and
    /// does not go forwards.
    #[test]
    fn a_bad_reading_holds_the_count_where_it_was() {
        let mut sensor = Fake::with_stages(3);
        let mut enrolment = Enrolment::begin(&sensor, Finger::LeftThumb).unwrap();
        sensor.will(&[Scan::Good, Scan::Retry(Retry::OffCentre), Scan::Good]);

        enrolment.step(&mut sensor).unwrap();
        assert_eq!(
            enrolment.step(&mut sensor).unwrap(),
            Progress::Again {
                why: Retry::OffCentre,
                done: 1,
                stages: 3,
                patience: PATIENCE - 1,
            }
        );
        assert_eq!(
            enrolment.step(&mut sensor).unwrap(),
            Progress::More {
                done: 2,
                stages: 3
            }
        );
    }

    /// Patience is a run, not a total: a finger that keeps working eventually
    /// enrols however many poor readings it took on the way.
    #[test]
    fn a_good_reading_restores_the_patience() {
        let mut sensor = Fake::with_stages(2);
        let mut enrolment = Enrolment::begin(&sensor, Finger::LeftIndex).unwrap();
        let mut script = vec![Scan::Retry(Retry::Unreadable); PATIENCE as usize - 1];
        script.push(Scan::Good);
        script.extend(vec![Scan::Retry(Retry::Unreadable); PATIENCE as usize - 1]);
        script.push(Scan::Good);
        sensor.will(&script);

        let mut last = Progress::GaveUp;
        for _ in 0..script.len() {
            last = enrolment.step(&mut sensor).unwrap();
        }
        assert_eq!(last, Progress::Enrolled(Finger::LeftIndex));
    }

    #[test]
    fn a_run_of_bad_readings_gives_up_and_leaves_no_half_template() {
        let mut sensor = Fake::with_stages(5);
        let mut enrolment = Enrolment::begin(&sensor, Finger::RightRing).unwrap();
        sensor.will(&vec![Scan::Retry(Retry::Unreadable); PATIENCE as usize]);

        let mut last = Progress::GaveUp;
        for _ in 0..PATIENCE {
            last = enrolment.step(&mut sensor).unwrap();
        }
        assert_eq!(last, Progress::GaveUp);
        assert!(enrolment.finished());
        assert!(sensor.abandoned(), "the sensor was left holding a template");
        assert!(sensor.enrolled().unwrap().is_empty());
    }

    /// A dialog that races its own reader thread gets the same answer twice
    /// rather than a panic or a second enrolment.
    #[test]
    fn stepping_past_the_end_repeats_the_verdict() {
        let mut sensor = Fake::with_stages(1);
        let mut enrolment = Enrolment::begin(&sensor, Finger::RightThumb).unwrap();
        sensor.will(&[Scan::Good]);
        assert_eq!(
            enrolment.step(&mut sensor).unwrap(),
            Progress::Enrolled(Finger::RightThumb)
        );
        assert_eq!(
            enrolment.step(&mut sensor).unwrap(),
            Progress::Enrolled(Finger::RightThumb)
        );
    }

    /// `NoMatch` means nothing while enrolling, and must not be allowed to
    /// look like a stage.
    #[test]
    fn a_sensor_that_reports_no_match_while_enrolling_is_not_believed() {
        let mut sensor = Fake::with_stages(2);
        let mut enrolment = Enrolment::begin(&sensor, Finger::LeftMiddle).unwrap();
        sensor.will(&[Scan::NoMatch]);
        assert!(matches!(
            enrolment.step(&mut sensor).unwrap(),
            Progress::Again { done: 0, .. }
        ));
    }

    #[test]
    fn a_sensor_that_wants_no_readings_is_refused() {
        let sensor = Fake::with_stages(0);
        assert!(Enrolment::begin(&sensor, Finger::RightIndex).is_err());
    }

    #[test]
    fn abandoning_tells_the_sensor_once() {
        let mut sensor = Fake::with_stages(5);
        let mut enrolment = Enrolment::begin(&sensor, Finger::LeftLittle).unwrap();
        enrolment.abandon(&mut sensor);
        enrolment.abandon(&mut sensor);
        assert_eq!(sensor.abandons(), 1);
    }
}
