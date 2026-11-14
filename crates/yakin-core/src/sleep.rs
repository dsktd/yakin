//! Noticing that the machine slept without being told: the boot clock runs through suspend, the
//! monotonic clock does not, so their difference jumps by the time slept.

use std::time::Duration;

/// A difference growing by more than this between two looks is a sleep.
const JUMP: Duration = Duration::from_secs(2);

/// What the two clocks last said.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sleeps {
    /// Boot minus monotonic at the last look; none before the first.
    offset: Option<Duration>,
}
impl Sleeps {
    /// Whether the machine slept since the last look, given both clocks now.
    pub fn slept(
        &mut self,
        boot: Duration,
        monotonic: Duration,
    ) -> bool {
        let offset = boot.saturating_sub(monotonic);
        let last = self.offset.replace(offset);
        last.is_some_and(|last| offset.saturating_sub(last) > JUMP)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn test_slept() {
        let mut sleeps = Sleeps::default();
        // The first look has nothing to compare with
        assert!(!sleeps.slept(secs(100), secs(90)));
        // Both clocks advance together while awake
        assert!(!sleeps.slept(secs(102), secs(92)));
        assert!(!sleeps.slept(secs(104), secs(94)));
        // Twelve seconds asleep: boot ran on, monotonic did not
        assert!(sleeps.slept(secs(120), secs(96)));
        // Seen once
        assert!(!sleeps.slept(secs(122), secs(98)));
        // A short nap under the jump goes unnoticed
        assert!(!sleeps.slept(secs(125), secs(100)));
    }
}
