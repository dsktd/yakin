//! The sleep timer's state, shared by `yakin-cli timer` (which sets it) and the status card
//! (which shows what is left): one line in the session's runtime dir, `<deadline> <minutes>`,
//! the deadline in Unix seconds. A systemd user timer does the suspending.

use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

/// The lengths `next` steps through, in minutes, then off.
pub const STEPS: [u32; 3] = [30, 60, 90];

/// The transient user unit that suspends: `yakin-sleep-timer.timer`.
pub const UNIT: &str = "yakin-sleep-timer";

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SleepTimer {
    /// Unix seconds when the machine suspends.
    pub deadline: u64,
    /// The length it was set to.
    pub minutes: u32,
}
impl SleepTimer {
    pub fn new(
        now: u64,
        minutes: u32,
    ) -> Self {
        Self {
            deadline: now + minutes as u64 * 60,
            minutes,
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        let (deadline, minutes) = line.trim().split_once(' ')?;
        Some(Self {
            deadline: deadline.parse().ok()?,
            minutes: minutes.parse().ok()?,
        })
    }

    /// Time left at `now`; none once it is past.
    pub fn left(
        &self,
        now: u64,
    ) -> Option<Duration> {
        (self.deadline > now).then(|| Duration::from_secs(self.deadline - now))
    }

    /// The length after this one's, none past the last: what `next` sets.
    pub fn next(timer: Option<Self>) -> Option<u32> {
        match timer {
            None => Some(STEPS[0]),
            Some(timer) => STEPS.iter().copied().find(|step| *step > timer.minutes),
        }
    }

    /// The running timer, none when unset or already past.
    pub fn read() -> Option<Self> {
        let timer = Self::parse(&std::fs::read_to_string(path()).ok()?)?;
        timer.left(now())?;
        Some(timer)
    }
}
impl std::fmt::Display for SleepTimer {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "{} {}", self.deadline, self.minutes)
    }
}

/// `$XDG_RUNTIME_DIR/yakin-sleep-timer`, else under `/tmp`.
pub fn path() -> PathBuf {
    match dirs::runtime_dir() {
        Some(dir) => dir.join(UNIT),
        None => PathBuf::from(format!("/tmp/{UNIT}-{}", nix::unistd::getuid())),
    }
}

/// Unix seconds now.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_round_trip_and_left() {
        let timer = SleepTimer::new(1000, 30);
        assert_eq!(SleepTimer::parse(&format!("{timer}\n")), Some(timer));
        assert_eq!(timer.left(1000), Some(Duration::from_secs(1800)));
        assert_eq!(timer.left(2800), None);
        assert_eq!(SleepTimer::parse("garbage"), None);
    }

    #[test]
    fn test_next_steps_then_off() {
        let at = |minutes| Some(SleepTimer::new(0, minutes));
        assert_eq!(SleepTimer::next(None), Some(30));
        assert_eq!(SleepTimer::next(at(30)), Some(60));
        assert_eq!(SleepTimer::next(at(60)), Some(90));
        assert_eq!(SleepTimer::next(at(90)), None);
    }
}
