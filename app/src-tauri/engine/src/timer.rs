//! The coding countdown: work_minutes of focus before dues are owed.

pub struct CodingTimer {
    duration: f64,
    deadline: Option<f64>,
}

impl CodingTimer {
    pub fn new(duration_secs: f64) -> Self {
        Self { duration: duration_secs, deadline: None }
    }

    pub fn start(&mut self, now: f64) {
        self.deadline = Some(now + self.duration);
    }

    /// Move the deadline forward when activity is paused or the machine slept.
    pub fn defer(&mut self, seconds: f64) {
        if let Some(deadline) = self.deadline.as_mut() { *deadline += seconds.max(0.0); }
    }

    pub fn configure(&mut self, duration: f64, now: f64) {
        self.duration = duration;
        self.start(now);
    }

    /// Run for `secs` from `now` without changing the configured interval
    /// (resuming a paused countdown).
    pub fn start_for(&mut self, now: f64, secs: f64) {
        self.deadline = Some(now + secs.max(0.0));
    }

    pub fn stop(&mut self) {
        self.deadline = None;
    }

    pub fn remaining(&self, now: f64) -> f64 {
        match self.deadline {
            Some(d) => (d - now).max(0.0),
            None => self.duration,
        }
    }

    pub fn expired(&self, now: f64) -> bool {
        matches!(self.deadline, Some(d) if now >= d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_down_and_expires() {
        let mut t = CodingTimer::new(360.0);
        assert_eq!(t.remaining(50.0), 360.0); // not started
        assert!(!t.expired(50.0));
        t.start(100.0);
        assert_eq!(t.remaining(100.0), 360.0);
        assert_eq!(t.remaining(160.0), 300.0);
        assert!(!t.expired(459.9));
        assert!(t.expired(460.0));
        assert_eq!(t.remaining(500.0), 0.0);
    }

    #[test]
    fn stop_disarms() {
        let mut t = CodingTimer::new(360.0);
        t.start(0.0);
        t.stop();
        assert!(!t.expired(1000.0));
        assert_eq!(t.remaining(1000.0), 360.0);
    }
}

#[cfg(test)] mod passive_tests {
 use super::*;
 #[test] fn pause_and_sleep_preserve_remaining() {
  let mut timer=CodingTimer::new(1500.);timer.start(100.);
  assert_eq!(timer.remaining(200.),1400.);
  timer.defer(3600.);assert_eq!(timer.remaining(3800.),1400.);
  assert!(!timer.expired(5199.));assert!(timer.expired(5200.));
 }
}
