//! The power of the TV, with one owner. yakin acts on what it observes, not on what it did: the
//! TV's power is a [`Belief`] with an age and a source, a belief older than its horizon is asked
//! about again, and unknown passes input to the desktop, only a confirmed standby swallows it.
//! Pure values: facts and reports in, [`Command`]s out; the engine runs them and reports back.
//! See `docs/design/input-power.md`.

use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use bincode::Decode;
use bincode::Encode;

use crate::Result;
use crate::output::Output;
use crate::state::Button;
use crate::state::ControllerState;

/// How long a command may go unanswered before the TV is asked about instead.
const PENDING: Duration = Duration::from_secs(3);

/// A value yakin observed, with when and from where.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Belief<T> {
    Unknown {
        since: Instant,
    },
    Known {
        value: T,
        since: Instant,
        source: Source,
    },
    /// A command was sent; `wanted` is not a fact until observed.
    Pending {
        wanted: T,
        since: Instant,
    },
}
impl<T: Copy + Eq> Belief<T> {
    /// The value, when known.
    pub fn value(&self) -> Option<T> {
        match self {
            Belief::Known { value, .. } => Some(*value),
            Belief::Unknown { .. } | Belief::Pending { .. } => None,
        }
    }

    pub fn since(&self) -> Instant {
        match self {
            Belief::Unknown { since }
            | Belief::Known { since, .. }
            | Belief::Pending { since, .. } => *since,
        }
    }

    pub fn age(
        &self,
        now: Instant,
    ) -> Duration {
        now.saturating_duration_since(self.since())
    }

    /// Unknown from `now` if it is known and older than `horizon`; as it was otherwise.
    pub fn decayed(
        self,
        horizon: Duration,
        now: Instant,
    ) -> Self {
        match self {
            Belief::Known { .. } if self.age(now) >= horizon => Belief::Unknown { since: now },
            belief => belief,
        }
    }
}

/// Where a belief came from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Source {
    /// The `tv_query` command found the TV on.
    Query,
    /// yakin's own command, or the word of another client: nothing heard back from the TV.
    Hook,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TvPower {
    On,
    Standby,
}

/// The machine suspending or coming back, as logind's `PrepareForSleep` says it.
#[derive(Encode, Decode, Copy, Clone, Debug, Eq, PartialEq)]
pub enum SleepPhase {
    Preparing,
    Resumed,
}
impl SleepPhase {
    /// `pre` or `post`, as systemd passes them to a `system-sleep` hook.
    pub fn parse(phase: &str) -> Result<Self> {
        match phase {
            "pre" => Ok(Self::Preparing),
            "post" => Ok(Self::Resumed),
            _ => Err(anyhow!("Sleep phase {phase:?} is not pre or post")),
        }
    }
}

/// What the outside world tells the power owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Fact {
    TvPower(TvPower, Source),
    Sleep(SleepPhase),
    /// A command ran; every command answers exactly once.
    Outcome(Command, Outcome),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Done,
    Failed,
}

/// A haptic or tone telling the user something, on every controller.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Cue {
    /// The TV did not do what was asked, or something that was to power down did not.
    Failed,
}

/// What the engine runs for the owner.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Ask whether the TV is on; the answer comes back as a [`Fact::TvPower`].
    TvQuery,
    /// Wake the TV and make this the active source.
    TvWake,
    TvStandby,
    /// The TV is on: make settop the active source.
    TvActiveSource,
    /// Power every controller down and drop its Bluetooth link, so it sleeps with the TV.
    ControllersOff,
    Say(Cue),
}

/// What the config decides for the owner.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Settings {
    /// How long a belief stands before it is asked about again; none when nothing can be asked.
    pub horizon: Option<Duration>,
    /// The TV goes to standby when the machine suspends.
    pub sleep_standby: bool,
}

/// The TV's power as yakin believes it, and the gesture that wakes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tv {
    power: Belief<TvPower>,
    settings: Settings,
    /// While in standby, when the current hold began (pressed in standby, so a button still
    /// held from the `tv:off` press does not count).
    hold: Option<Instant>,
    /// Swallowing until every button is up, after a wake.
    draining: bool,
}
impl Tv {
    /// A TV of unknown power, and the query that starts finding out: a restart assumes nothing.
    pub fn new(
        now: Instant,
        settings: Settings,
    ) -> (Self, Vec<Command>) {
        let tv = Self {
            power: Belief::Unknown { since: now },
            settings,
            hold: None,
            draining: false,
        };
        (tv, vec![Command::TvQuery])
    }

    /// Change the settings, after a config reload.
    pub fn configure(
        &mut self,
        settings: Settings,
    ) {
        self.settings = settings;
    }

    /// Whether every report is swallowed: a confirmed standby, a command waiting to be
    /// answered, or the rest of a wake press.
    pub fn gating(&self) -> bool {
        self.draining
            || matches!(
                self.power,
                Belief::Known {
                    value: TvPower::Standby,
                    ..
                } | Belief::Pending { .. }
            )
    }

    /// Take in what was observed.
    pub fn fact(
        &mut self,
        fact: &Fact,
        now: Instant,
    ) -> Vec<Command> {
        match *fact {
            Fact::TvPower(value, source) => {
                self.power = Belief::Known {
                    value,
                    since: now,
                    source,
                };
                self.hold = None;
            }
            // The controllers go down so they cannot wake the machine, and the TV with it. The
            // TV is not asked now, nothing would answer before the machine is gone
            Fact::Sleep(SleepPhase::Preparing) => {
                let standby = self.settings.sleep_standby
                    && !matches!(
                        self.power,
                        Belief::Known {
                            value: TvPower::Standby,
                            ..
                        } | Belief::Pending {
                            wanted: TvPower::Standby,
                            ..
                        }
                    );
                self.forget(now);
                let mut commands = vec![Command::ControllersOff];
                if standby {
                    commands.push(Command::TvStandby);
                }
                return commands;
            }
            // Nothing is known of the TV after a sleep: pass input and ask
            Fact::Sleep(SleepPhase::Resumed) => {
                self.forget(now);
                return vec![Command::TvQuery];
            }
            Fact::Outcome(Command::ControllersOff, Outcome::Failed) => {
                return vec![Command::Say(Cue::Failed)];
            }
            Fact::Outcome(command, outcome) => {
                let Belief::Pending { wanted, .. } = self.power else {
                    return Vec::new();
                };
                let answers = match command {
                    Command::TvWake => wanted == TvPower::On,
                    Command::TvStandby => wanted == TvPower::Standby,
                    _ => false,
                };
                match outcome {
                    // The hook is all that is heard back: a command that ran is taken as seen
                    Outcome::Done if answers => {
                        self.fact(&Fact::TvPower(wanted, Source::Hook), now);
                    }
                    Outcome::Failed if answers => return self.lost(now),
                    _ => {}
                }
            }
        }
        Vec::new()
    }

    /// Believe nothing of the TV from `now`.
    fn forget(
        &mut self,
        now: Instant,
    ) {
        self.power = Belief::Unknown { since: now };
        self.hold = None;
    }

    /// The TV did not answer: unknown again, which asks it, and the user is told.
    fn lost(
        &mut self,
        now: Instant,
    ) -> Vec<Command> {
        self.forget(now);
        vec![Command::TvQuery, Command::Say(Cue::Failed)]
    }

    /// A controller connected. A connect is no press: it never wakes the TV, it makes settop the
    /// active source when the TV is on, and asks again while a standby is believed (the TV may
    /// have been turned on by its remote).
    pub fn controller_connected(&mut self) -> Vec<Command> {
        match self.power.value() {
            Some(TvPower::On) => vec![Command::TvActiveSource],
            Some(TvPower::Standby) => vec![Command::TvQuery],
            None => Vec::new(),
        }
    }

    /// The outputs of one report from `prev` to `state` at `now`, and what it switched. In
    /// standby only a button (not a touch) held for `wake` gets through, as a wake.
    pub fn filter(
        &mut self,
        prev: &ControllerState,
        state: &ControllerState,
        outputs: Vec<Output>,
        now: Instant,
        wake: Duration,
    ) -> (Vec<Output>, Vec<Command>) {
        let held = |state: &ControllerState, button: Button| {
            !button.is_touch() && state.buttons.contains(button)
        };
        let any = Button::ALL.into_iter().any(|button| held(state, button));
        if self.draining {
            self.draining = any;
            return (Vec::new(), Vec::new());
        }
        if matches!(self.power, Belief::Pending { .. }) {
            return (Vec::new(), Vec::new());
        }
        if self.power.value() != Some(TvPower::Standby) {
            // The rest of the report still goes out: a release in it would be lost otherwise
            if !outputs.contains(&Output::TvOff) {
                return (outputs, Vec::new());
            }
            let outputs = outputs
                .into_iter()
                .filter(|output| *output != Output::TvOff)
                .collect();
            self.power = Belief::Pending {
                wanted: TvPower::Standby,
                since: now,
            };
            self.hold = None;
            return (outputs, vec![Command::ControllersOff, Command::TvStandby]);
        }
        let pressed = Button::ALL
            .into_iter()
            .any(|button| !held(prev, button) && held(state, button));
        self.hold = match self.hold {
            _ if !any => None,
            None if pressed => Some(now),
            hold => hold,
        };
        if self.hold.is_some_and(|since| now >= since + wake) {
            self.draining = true;
            self.power = Belief::Pending {
                wanted: TvPower::On,
                since: now,
            };
            self.hold = None;
            return (Vec::new(), vec![Command::TvWake]);
        }
        (Vec::new(), Vec::new())
    }

    /// A command unanswered for [`PENDING`] makes the TV unknown. Ask again about a belief past
    /// its horizon: a query can only prove the TV on, so a standby stays believed while it is
    /// asked about; any other belief goes unknown, which passes input meanwhile.
    pub fn tick(
        &mut self,
        now: Instant,
    ) -> Vec<Command> {
        if matches!(self.power, Belief::Pending { .. }) {
            return if self.power.age(now) < PENDING {
                Vec::new()
            } else {
                self.lost(now)
            };
        }
        let Some(horizon) = self.settings.horizon else {
            return Vec::new();
        };
        if self.power.age(now) < horizon {
            return Vec::new();
        }
        self.power = match self.power {
            Belief::Known {
                value: TvPower::Standby,
                source,
                ..
            } => Belief::Known {
                value: TvPower::Standby,
                since: now,
                source,
            },
            Belief::Unknown { .. } => Belief::Unknown { since: now },
            known => known.decayed(horizon, now),
        };
        vec![Command::TvQuery]
    }

    /// When [`Tv::tick`] has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        let since = self.power.since();
        match self.power {
            Belief::Pending { .. } => Some(since + PENDING),
            _ => self.settings.horizon.map(|horizon| since + horizon),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const WAKE: Duration = Duration::from_millis(1000);
    const HORIZON: Duration = Duration::from_secs(300);

    fn settings(horizon: Option<Duration>) -> Settings {
        Settings {
            horizon,
            sleep_standby: true,
        }
    }

    fn pressed(buttons: &[Button]) -> ControllerState {
        let mut state = ControllerState::default();
        for button in buttons {
            state.buttons = state.buttons.with(*button);
        }
        state
    }

    fn key() -> Output {
        Output::Key(30, true)
    }

    fn at(
        t0: Instant,
        ms: u64,
    ) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// A TV of known power, believed since the returned instant.
    fn known(power: TvPower) -> (Tv, Instant) {
        let t0 = Instant::now();
        let (mut tv, _) = Tv::new(t0, settings(Some(HORIZON)));
        tv.fact(&Fact::TvPower(power, Source::Query), t0);
        (tv, t0)
    }

    /// A gate put to standby at the returned instant.
    fn off() -> (Tv, Instant) {
        let (mut tv, t0) = known(TvPower::On);
        let idle = ControllerState::default();
        assert_eq!(
            tv.filter(&idle, &idle, vec![Output::TvOff], t0, WAKE),
            (vec![], vec![Command::ControllersOff, Command::TvStandby])
        );
        assert!(tv.gating());
        tv.fact(&done(Command::TvStandby), at(t0, 10));
        (tv, t0)
    }

    fn done(command: Command) -> Fact {
        Fact::Outcome(command, Outcome::Done)
    }

    #[test]
    fn test_restart_assumes_nothing() {
        let t0 = Instant::now();
        let (mut tv, commands) = Tv::new(t0, settings(Some(HORIZON)));
        assert_eq!(commands, vec![Command::TvQuery]);
        assert_eq!(tv.power, Belief::Unknown { since: t0 });
        assert!(!tv.gating());
        let idle = ControllerState::default();
        assert_eq!(
            tv.filter(&idle, &idle, vec![key()], t0, WAKE),
            (vec![key()], vec![])
        );
    }

    #[test]
    fn test_unknown_passes_and_can_go_to_standby() {
        let t0 = Instant::now();
        let (mut tv, _) = Tv::new(t0, settings(None));
        let idle = ControllerState::default();
        let release = Output::Key(30, false);
        // The rest of the standby report still goes out
        assert_eq!(
            tv.filter(&idle, &idle, vec![release.clone(), Output::TvOff], t0, WAKE),
            (
                vec![release],
                vec![Command::ControllersOff, Command::TvStandby]
            )
        );
        assert!(tv.gating());
    }

    #[test]
    fn test_connect_never_wakes() {
        let (mut tv, t0) = off();
        // Standby stays, and the TV is asked again
        assert_eq!(tv.controller_connected(), vec![Command::TvQuery]);
        assert!(tv.gating());
        // Unknown does nothing: the query at start is on its way
        let (mut tv, _) = Tv::new(t0, settings(None));
        assert_eq!(tv.controller_connected(), vec![]);
        // On makes settop the active source
        let (mut tv, _) = known(TvPower::On);
        assert_eq!(tv.controller_connected(), vec![Command::TvActiveSource]);
    }

    #[test]
    fn test_reconnect_after_standby_does_not_wake() {
        // Row 1a
        let (mut tv, t0) = off();
        assert!(!tv.controller_connected().contains(&Command::TvWake));
        assert!(!tv.controller_connected().contains(&Command::TvActiveSource));
        let idle = ControllerState::default();
        assert_eq!(
            tv.filter(&idle, &idle, vec![key()], at(t0, 100), WAKE),
            (vec![], vec![])
        );
    }

    #[test]
    fn test_failed_standby_is_heard_and_asked_about() {
        let (mut tv, t0) = known(TvPower::On);
        let idle = ControllerState::default();
        tv.filter(&idle, &idle, vec![Output::TvOff], t0, WAKE);
        assert_eq!(
            tv.fact(
                &Fact::Outcome(Command::TvStandby, Outcome::Failed),
                at(t0, 100)
            ),
            vec![Command::TvQuery, Command::Say(Cue::Failed)]
        );
        assert!(!tv.gating());
    }

    #[test]
    fn test_failed_disconnect_is_a_cue_and_nothing_else() {
        // Row 1b: the standby is still waited for, then times out to unknown
        let (mut tv, t0) = known(TvPower::On);
        let idle = ControllerState::default();
        tv.filter(&idle, &idle, vec![Output::TvOff], t0, WAKE);
        let failed = Fact::Outcome(Command::ControllersOff, Outcome::Failed);
        assert_eq!(
            tv.fact(&failed, at(t0, 100)),
            vec![Command::Say(Cue::Failed)]
        );
        assert!(tv.gating());
        assert_eq!(tv.deadline(), Some(t0 + PENDING));
        assert_eq!(tv.tick(t0 + PENDING - Duration::from_millis(1)), vec![]);
        assert_eq!(
            tv.tick(t0 + PENDING),
            vec![Command::TvQuery, Command::Say(Cue::Failed)]
        );
        assert!(!tv.gating());
    }

    #[test]
    fn test_wake_never_confirmed_gives_up() {
        // Row 3c: input passes after the 3 s, and the hold must be repeated
        let (mut tv, t0) = off();
        let idle = ControllerState::default();
        let a = pressed(&[Button::A]);
        tv.filter(&idle, &a, vec![], at(t0, 100), WAKE);
        let woke = at(t0, 1100);
        assert_eq!(
            tv.filter(&a, &a, vec![], woke, WAKE),
            (vec![], vec![Command::TvWake])
        );
        assert_eq!(
            tv.tick(woke + PENDING),
            vec![Command::TvQuery, Command::Say(Cue::Failed)]
        );
        tv.filter(&a, &idle, vec![], woke + PENDING, WAKE);
        assert_eq!(
            tv.filter(&idle, &idle, vec![key()], woke + PENDING, WAKE),
            (vec![key()], vec![])
        );
    }

    #[test]
    fn test_late_outcome_is_ignored() {
        let (mut tv, t0) = known(TvPower::On);
        tv.fact(&done(Command::TvWake), t0);
        assert_eq!(tv.power.value(), Some(TvPower::On));
        let idle = ControllerState::default();
        tv.filter(&idle, &idle, vec![Output::TvOff], t0, WAKE);
        // A wake's outcome does not answer a standby
        tv.fact(&done(Command::TvWake), t0);
        assert!(matches!(tv.power, Belief::Pending { .. }));
    }

    #[test]
    fn test_suspend_powers_everything_down() {
        // Row 4
        let (mut tv, t0) = known(TvPower::On);
        assert_eq!(
            tv.fact(&Fact::Sleep(SleepPhase::Preparing), at(t0, 100)),
            vec![Command::ControllersOff, Command::TvStandby]
        );
        assert_eq!(tv.power, Belief::Unknown { since: at(t0, 100) });
        assert!(!tv.gating());
        // The TV is not told twice, nor when yakin was told not to
        let (mut tv, t0) = off();
        assert_eq!(
            tv.fact(&Fact::Sleep(SleepPhase::Preparing), at(t0, 100)),
            vec![Command::ControllersOff]
        );
        let (mut tv, _) = Tv::new(
            t0,
            Settings {
                horizon: None,
                sleep_standby: false,
            },
        );
        tv.fact(&Fact::TvPower(TvPower::On, Source::Query), t0);
        assert_eq!(
            tv.fact(&Fact::Sleep(SleepPhase::Preparing), at(t0, 100)),
            vec![Command::ControllersOff]
        );
    }

    #[test]
    fn test_resume_forgets_and_asks_with_input_passing() {
        // Row 3a, as far as the owner goes
        let (mut tv, t0) = off();
        let idle = ControllerState::default();
        assert_eq!(
            tv.fact(&Fact::Sleep(SleepPhase::Resumed), at(t0, 500)),
            vec![Command::TvQuery]
        );
        assert!(!tv.gating());
        assert_eq!(
            tv.filter(&idle, &idle, vec![key()], at(t0, 600), WAKE),
            (vec![key()], vec![])
        );
        // Neither a connect nor the standby's late outcome wakes or gates it
        assert_eq!(tv.controller_connected(), vec![]);
        tv.fact(&done(Command::TvStandby), at(t0, 700));
        assert!(!tv.gating());
    }

    #[test]
    fn test_sleep_phase_parse() {
        assert_eq!(SleepPhase::parse("pre").unwrap(), SleepPhase::Preparing);
        assert_eq!(SleepPhase::parse("post").unwrap(), SleepPhase::Resumed);
        assert!(SleepPhase::parse("suspend").is_err());
    }

    #[test]
    fn test_known_on_passes_through() {
        let (mut tv, t0) = known(TvPower::On);
        let idle = ControllerState::default();
        assert_eq!(
            tv.filter(&idle, &idle, vec![key()], t0, WAKE),
            (vec![key()], vec![])
        );
    }

    #[test]
    fn test_fact_standby_gates_like_tv_off() {
        let (mut tv, t0) = known(TvPower::On);
        tv.fact(&Fact::TvPower(TvPower::Standby, Source::Hook), t0);
        assert!(tv.gating());
        // A held button wakes it as after tv:off
        let idle = ControllerState::default();
        let a = pressed(&[Button::A]);
        assert_eq!(tv.filter(&idle, &a, Vec::new(), t0, WAKE), (vec![], vec![]));
        assert_eq!(
            tv.filter(&a, &a, Vec::new(), at(t0, 1000), WAKE),
            (vec![], vec![Command::TvWake])
        );
    }

    #[test]
    fn test_short_tap_stays_idle() {
        let (mut tv, t0) = off();
        let idle = ControllerState::default();
        let grip = pressed(&[Button::GripRTouch]);
        let a = pressed(&[Button::A]);
        let none = (vec![], vec![]);
        // Touches never count, however long
        assert_eq!(tv.filter(&idle, &grip, vec![key()], at(t0, 0), WAKE), none);
        assert_eq!(
            tv.filter(&grip, &grip, vec![key()], at(t0, 2000), WAKE),
            none
        );
        assert_eq!(tv.filter(&grip, &a, vec![key()], at(t0, 2100), WAKE), none);
        assert_eq!(tv.filter(&a, &a, vec![key()], at(t0, 2500), WAKE), none);
        assert_eq!(tv.filter(&a, &idle, vec![key()], at(t0, 2600), WAKE), none);
        // The released tap leaves no timer behind
        assert_eq!(tv.filter(&idle, &idle, vec![], at(t0, 5000), WAKE), none);
        assert!(tv.gating());
    }

    #[test]
    fn test_hold_wakes_once_and_release_is_swallowed() {
        let (mut tv, t0) = off();
        let idle = ControllerState::default();
        let a = pressed(&[Button::A]);
        let none = (vec![], vec![]);
        assert_eq!(tv.filter(&idle, &a, vec![key()], at(t0, 100), WAKE), none);
        assert_eq!(tv.filter(&a, &a, vec![key()], at(t0, 1099), WAKE), none);
        assert_eq!(
            tv.filter(&a, &a, vec![key()], at(t0, 1100), WAKE),
            (vec![], vec![Command::TvWake])
        );
        tv.fact(&done(Command::TvWake), at(t0, 1200));
        assert_eq!(tv.filter(&a, &a, vec![key()], at(t0, 3000), WAKE), none);
        assert_eq!(tv.filter(&a, &idle, vec![key()], at(t0, 3100), WAKE), none);
        assert!(!tv.gating());
        assert_eq!(
            tv.filter(&idle, &a, vec![key()], at(t0, 3200), WAKE),
            (vec![key()], vec![])
        );
    }

    #[test]
    fn test_second_button_keeps_the_hold() {
        let (mut tv, t0) = off();
        let idle = ControllerState::default();
        let a = pressed(&[Button::A]);
        let ab = pressed(&[Button::A, Button::B]);
        let b = pressed(&[Button::B]);
        let none = (vec![], vec![]);
        assert_eq!(tv.filter(&idle, &a, vec![], at(t0, 0), WAKE), none);
        // B joining and A leaving keep timing from A's press
        assert_eq!(tv.filter(&a, &ab, vec![], at(t0, 500), WAKE), none);
        assert_eq!(tv.filter(&ab, &b, vec![], at(t0, 800), WAKE), none);
        assert_eq!(
            tv.filter(&b, &b, vec![], at(t0, 1000), WAKE),
            (vec![], vec![Command::TvWake])
        );
        tv.fact(&done(Command::TvWake), at(t0, 1100));
        // Swallowed until both are up, even with A pressed again
        assert_eq!(tv.filter(&b, &ab, vec![key()], at(t0, 1200), WAKE), none);
        assert_eq!(tv.filter(&ab, &a, vec![key()], at(t0, 1300), WAKE), none);
        assert_eq!(tv.filter(&a, &idle, vec![key()], at(t0, 1400), WAKE), none);
        assert!(!tv.gating());
    }

    #[test]
    fn test_held_through_standby_does_not_wake() {
        let (mut tv, t0) = known(TvPower::On);
        let menu = pressed(&[Button::Menu]);
        tv.filter(&menu, &menu, vec![Output::TvOff], t0, WAKE);
        tv.fact(&done(Command::TvStandby), at(t0, 10));
        assert_eq!(
            tv.filter(&menu, &menu, vec![], at(t0, 5000), WAKE),
            (vec![], vec![])
        );
        assert!(tv.gating());
    }

    #[test]
    fn test_found_on_lets_input_through_without_a_wake() {
        let (mut tv, t0) = off();
        let idle = ControllerState::default();
        let steam = pressed(&[Button::Steam]);
        tv.fact(&Fact::TvPower(TvPower::On, Source::Query), at(t0, 50));
        assert!(!tv.gating());
        assert_eq!(
            tv.filter(&idle, &steam, vec![key()], at(t0, 100), WAKE),
            (vec![key()], vec![])
        );
    }

    #[test]
    fn test_on_goes_unknown_at_the_horizon_and_asks() {
        let (mut tv, t0) = known(TvPower::On);
        assert_eq!(tv.deadline(), Some(t0 + HORIZON));
        assert_eq!(tv.tick(t0 + HORIZON - Duration::from_millis(1)), vec![]);
        let later = t0 + HORIZON;
        assert_eq!(tv.tick(later), vec![Command::TvQuery]);
        assert_eq!(tv.power, Belief::Unknown { since: later });
        // Still unknown a horizon later: asked again, not forgotten
        assert_eq!(tv.tick(later + HORIZON), vec![Command::TvQuery]);
    }

    #[test]
    fn test_standby_not_heard_is_caught_by_the_next_query() {
        // The TV was turned on by its remote and yakin heard nothing (row 2b)
        let (mut tv, t0) = off();
        let idle = ControllerState::default();
        let later = at(t0, 10) + HORIZON;
        assert_eq!(tv.tick(later), vec![Command::TvQuery]);
        // Silence keeps the gate, so the wake hold still works
        assert!(tv.gating());
        tv.fact(&Fact::TvPower(TvPower::On, Source::Query), later);
        assert_eq!(
            tv.filter(&idle, &idle, vec![key()], later, WAKE),
            (vec![key()], vec![])
        );
    }

    #[test]
    fn test_no_horizon_never_asks() {
        let t0 = Instant::now();
        let (mut tv, _) = Tv::new(t0, settings(None));
        tv.fact(&Fact::TvPower(TvPower::Standby, Source::Hook), t0);
        assert_eq!(tv.deadline(), None);
        assert_eq!(tv.tick(t0 + HORIZON * 10), vec![]);
        assert!(tv.gating());
    }

    /// A tiny deterministic generator, so a failure replays.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn test_never_hung() {
        let buttons = [Button::A, Button::B, Button::Menu, Button::GripRTouch];
        for seed in 1..=4u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let t0 = Instant::now();
            let horizon = seed.is_multiple_of(2).then_some(HORIZON);
            let (mut tv, _) = Tv::new(t0, settings(horizon));
            let (mut now, mut prev) = (t0, ControllerState::default());
            for _ in 0..10_000 {
                now += Duration::from_millis(rng.next() % 3000);
                match rng.next() % 5 {
                    0 => {
                        let power = [TvPower::On, TvPower::Standby][(rng.next() % 2) as usize];
                        tv.fact(&Fact::TvPower(power, Source::Query), now);
                        let command =
                            [Command::TvWake, Command::TvStandby][(rng.next() % 2) as usize];
                        if rng.next().is_multiple_of(9) {
                            let phase = [SleepPhase::Preparing, SleepPhase::Resumed]
                                [(rng.next() % 2) as usize];
                            tv.fact(&Fact::Sleep(phase), now);
                        }
                        let outcome = [Outcome::Done, Outcome::Failed][(rng.next() % 2) as usize];
                        tv.fact(&Fact::Outcome(command, outcome), now);
                    }
                    1 => {
                        tv.tick(now);
                    }
                    _ => {
                        let pick = buttons.iter().filter(|_| rng.next().is_multiple_of(2));
                        let state = pressed(&pick.copied().collect::<Vec<_>>());
                        let outputs = if rng.next().is_multiple_of(4) {
                            vec![Output::TvOff]
                        } else {
                            vec![key()]
                        };
                        tv.filter(&prev, &state, outputs, now, WAKE);
                        prev = state;
                    }
                }
            }
            // From wherever it ended, once an unanswered command gave up, one release and one
            // hold of a button gets out
            now += PENDING;
            tv.tick(now);
            let idle = ControllerState::default();
            let a = pressed(&[Button::A]);
            tv.filter(&prev, &idle, vec![], now, WAKE);
            tv.filter(&idle, &idle, vec![], now, WAKE);
            tv.filter(&idle, &a, vec![], now, WAKE);
            now += WAKE;
            tv.filter(&a, &a, vec![], now, WAKE);
            tv.fact(&done(Command::TvWake), now);
            tv.filter(&a, &idle, vec![], now, WAKE);
            assert_eq!(
                tv.filter(&idle, &a, vec![key()], now, WAKE),
                (vec![key()], vec![]),
                "seed {seed} still swallowing"
            );
        }
    }
}
