# Input and power: facts, beliefs, and one owner each

Design, 2026-11-07. Migration steps 1 to 3 are implemented; the notes under "Implementation
notes" say where the code differs from the text.

## Why

Eight bugs since the TV gate landed, all one bug: yakin acts on what it _did_ (I sent Standby,
so the TV is off) instead of what it _observes_, and nothing ever corrects the belief except a
restart or a user gesture. Each fix so far added a branch to `Engine` or `Tv`:

| #   | Symptom                                                                                            | Root cause in today's code                                                                                                    |
| --- | -------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| 1   | Self-reconnect after `tv:off` woke the TV                                                          | a connect was treated as intent (`power(Wake)`); `bluetoothctl` not on PATH made the disconnect silently fail                 |
| 2   | TV on by remote, yakin swallowed input until a 1 s hold                                            | `Gate::Off` is sticky; `tv_query` at connect is a one-shot patch                                                              |
| 3   | After resume: CEC unconfigured (log addr mask 0, phys f.f.f.f), wake sent as Unregistered, ignored | nobody owns the CEC adapter's state; `cec-wake` polls 5 s then gives up; a 30 s timer re-claims out of band                   |
| 4   | Spurious BT wake from suspend with the controller connected                                        | no notion of sleep at all; controllers are not powered down before suspend                                                    |
| 5   | Lizard leaks on every reconnect, setup retries                                                     | setup is a side quest of the read thread; the engine learns the outcome late (`Event::Lizard`) and patches (`watch`, `leaks`) |
| 6   | `tv:off` timing, warn beep, wake-by-hold, chord/layer edge cases, chord lists replace              | the gate, the mapper and the config each own a piece of "what does a held button mean"                                        |
| 7   | Restart fixes everything                                                                           | `Tv::default()` is `On`: the only path to "On" that does not need a gesture                                                   |
| 8   | Daemon looks hung                                                                                  | belief `Off`, reality on, input swallowed by design                                                                           |

The fix is structural: **observed facts are typed and owned; belief is derived from facts with
an age; every side effect is a command whose outcome comes back as a fact; nothing is sticky.**
Unknown is a first-class state and has a defined behaviour.

## Principles

1. **Facts in, commands out.** State machines are pure value types: `fn step(&mut self, input, now) -> Vec<Command>`. They never touch a device, a socket or a clock. This is what `Tv::filter` and `Lizard::filter` already are; the design finishes the job and moves every decision there.
2. **One owner per fact.** The TV's power is owned by `power::Tv`; the controller's lizard/IMU/power state by `session::Session`; the CEC adapter's address by `cec::Adapter`. Nobody else holds a copy; they ask.
3. **Belief has an age and a source.** `Known { value, since, source }` or `Unknown { since }`. A belief older than its horizon decays to `Unknown`, which triggers a query, never a guess.
4. **Intent is not state.** "I asked the TV to stand by" is `Pending { wanted, since }`. It becomes a fact only when observed (Report Power Status, or a Standby broadcast heard back). A timeout re-queries; it does not assume success.
5. **A connect is not a press.** Only a human gesture (a button held `tv_wake_ms`) wakes the TV. Reconnects, hotplug, resume and restarts never do.
6. **Unknown passes input.** When yakin does not know whether the TV is on, input goes to the desktop and a query is issued. A wrongly delivered click is cheap; a daemon that looks hung is not. The only swallowing state is `Standby` _confirmed_, and it decays.
7. **Executors are dumb, idempotent and report back.** "Make the TV on" can be sent twice; the executor transmits, waits for the observable outcome, and reports `Outcome`. No executor keeps policy.
8. **Keep Cal's shape.** Threads + `crossbeam-channel`, `anyhow`, `nix` ioctls (as `hidraw.rs`), tests only on pure types, no async, no trait objects: swap-able parts are generics with inline bounds or, better, pure machines that need no swapping.

## Layers

```
 facts/events                                            commands
 ───────────▶ ┌──────────────────────────────────────┐ ───────────▶
              │ 5 executors (threads, dumb)           │  CecAdapter, Bluetooth, Hidraw features,
              │   cec · bluetooth · hidraw · shell    │  shell hooks, uinput/desktop
              │   · sleep (system-sleep hook + clock) │
              └──────────────┬───────────────────────┘
                             │ Fact                      ▲ Command
              ┌──────────────▼───────────────────────┐   │
              │ 4 power::Tv  (belief + wake gesture)  │───┘   one owner of TV power
              ├──────────────────────────────────────┤
              │ 3 session::Session per controller     │       setup, lizard, power, link
              ├──────────────────────────────────────┤
              │ 2 input: Mapper, Layers, Chords,      │       pure; untouched by this design
              │   Menu, Keyboard                      │       except where noted in bug 6
              ├──────────────────────────────────────┤
              │ 1 transport: hidraw read loop, rescan │       delivers reports and link facts only
              └──────────────────────────────────────┘
 Engine = the wiring: one `match Event`, forwards to the owner, forwards commands to executors.
```

Report flow for one HID report: transport → `Session::report` (drops it while not set up or
lizard-handed) → `Mapper::map` → `Tv::filter` (gate) → `Lizard::filter` → executors. The order
is the same as today; what changes is who decides and from what.

## Facts, beliefs and commands (yakin-core, new module `power`)

```rust
/// A value yakin observed, with when and from where.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Belief<T> {
    Unknown { since: Instant },
    Known { value: T, since: Instant, source: Source },
    /// A command was sent; `wanted` is not a fact until observed.
    Pending { wanted: T, since: Instant },
}
impl<T: Copy + Eq> Belief<T> {
    pub fn value(&self) -> Option<T>;             // Known only
    pub fn age(&self, now: Instant) -> Duration;
    pub fn decayed(self, horizon: Duration, now: Instant) -> Self;  // Known past horizon -> Unknown
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Source { Report, Broadcast, Query, Hook, Restart }

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TvPower { On, Standby, Transition }   // Transition = CEC 0x02/0x03

/// What the outside world tells the power owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Fact {
    TvPower(TvPower, Source),             // Report Power Status, Standby broadcast, Image View On seen
    TvActiveSource(PhysAddr),            // who the TV shows; ours or not
    CecLink(Link),                       // from CEC_EVENT_STATE_CHANGE
    ControllerLink(ControllerId, LinkState), // Opened/Connected/Disconnected/Closed
    Sleep(SleepPhase),                   // Preparing, Resumed
    Outcome(CommandId, Outcome),         // every command answers exactly once
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Link {
    /// Adapter has a physical address and a claimed logical address: TV is reachable.
    Configured { phys: PhysAddr, logical: u8 },
    /// Physical address f.f.f.f or logical mask 0: HPD/EDID gone (Samsung in standby, resume).
    Unconfigured { phys: Option<PhysAddr> },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SleepPhase { Preparing, Resumed }

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    TvQuery,                             // Give Device Power Status
    TvWake,                              // Image View On + Active Source; see cec::Adapter for the unconfigured path
    TvStandby,
    TvActiveSource,                      // Active Source only (we are on, TV is on)
    CecClaim,                            // (re)claim Playback 1
    ControllerOff(ControllerId),         // feature report power-off
    BluetoothDisconnect(ControllerId),
    Shell(String),                       // the configured hook, for non-CEC setups
    Say(Cue),                            // haptic/tone to the controller the gesture came from
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Outcome { Done, Unreachable, Failed }
```

`Outcome::Unreachable` is the new word for bug 3: the executor could not get a frame onto the
bus from a claimed address. The owner then waits for `Fact::CecLink(Configured)` instead of
retrying blindly.

### `power::Tv`: the one owner

Replaces `tv::Tv`. Same shape (`filter` on a report), new inputs.

```rust
pub struct Tv {
    power: Belief<TvPower>,
    link: Link,
    /// A wake-by-hold in progress: pressed while believed Standby, with when.
    hold: Option<Instant>,
    /// Swallowing until every button is up, after a wake or a standby press.
    draining: bool,
    /// The last command we sent that is still unanswered.
    inflight: Option<(CommandId, Command, Instant)>,
}
impl Tv {
    pub fn new(now: Instant) -> Self;      // power = Unknown { since: now }: a restart queries, never assumes
    pub fn fact(&mut self, fact: &Fact, now: Instant) -> Vec<Command>;
    pub fn filter(&mut self, prev: &ControllerState, state: &ControllerState,
                  outputs: Vec<Output>, now: Instant, wake: Duration) -> (Vec<Output>, Vec<Command>);
    pub fn tick(&mut self, now: Instant) -> Vec<Command>;   // decay, timeouts; engine calls at `deadline()`
    pub fn deadline(&self) -> Option<Instant>;
    pub fn gating(&self) -> bool;        // Known(Standby) or Pending(Standby) only
}
```

State, as the engine sees it (derived from `power`):

```
                 ┌────────────────────────────────────────────────────────┐
                 │                                                        │
   start/restart │   Fact::TvPower(On)                 Fact::TvPower(Standby)
   sleep Resumed ▼   Outcome(Query)=Done+On           Standby broadcast heard
   decay      ┌─────────┐ ───────────────▶ ┌────┐ ───────────────────────▶ ┌─────────┐
   link lost  │ UNKNOWN │                  │ ON │                          │ STANDBY │
              └─────────┘ ◀─────────────── └────┘ ◀─────────────────────── └─────────┘
                  ▲  │  horizon (5 min) / link lost │  wake confirmed (On)        │
                  │  └─ emits TvQuery on entry      ▼                             │
                  │                       PENDING(Standby)            PENDING(On)   ◀─ hold ≥ tv_wake
                  │                       tv:off press / IPC off       re-sent on CecLink(Configured)
                  └─── timeout (3 s): back to UNKNOWN + TvQuery  ◀─────┘
 Gate:  UNKNOWN pass + query │ ON pass │ STANDBY swallow, hold wakes │ PENDING(*) swallow, drain
```

Rules, each the fix of one bug:

- Entry into `Unknown` always emits `TvQuery` (and `CecClaim` if `link` is `Unconfigured`). Entry happens at start (#7), on `Sleep(Resumed)` (#3/#4), on `CecLink(Unconfigured)` (#3), on horizon decay (#2/#8), on a command timeout.
- `Known(Standby)` has a horizon too (5 min, `tv_belief_s`), so a TV turned on by its remote while yakin heard nothing is caught by the next query (#2). With the CEC executor the remote case is caught instantly: Samsung broadcasts `Report Power Status`/`Active Source`/`Set Stream Path` on power-on, and the adapter sees them even as a non-destination.
- `ControllerLink(Connected)` never produces `TvWake`. It produces `TvActiveSource` when `Known(On)`, nothing otherwise (#1). A query is not needed on connect any more: the belief is kept current independently.
- A `TvWake` while `link` is `Unconfigured` is still issued (the TV may be HPD-less but listening), and the owner stays `Pending(On)`; on `CecLink(Configured)` it re-sends once from the claimed address (#3). Timeout 3 s → `Unknown` + query, and the hold must be repeated; the controller gets a `Cue::Failed` tone so the user knows it was heard.
- `Sleep(Preparing)` emits `ControllerOff(*)` and `BluetoothDisconnect(*)` and sets `power = Unknown`. Powered-down controllers do not wake the host (#4). Whether the TV is put to standby on suspend is config (`sleep_tv_off`, default true: suspend is the settop going away).
- `Pending`/`draining` swallow input but never for longer than the timeout. There is no state the user cannot leave with a 1 s hold or by waiting 3 s.

### What "the right thing" is when unknown

| Fact unknown            | Behaviour                                                                                                                                |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| TV power                | pass input through, `TvQuery` once per entry, re-query at `tv_belief_s`                                                                  |
| CEC link                | `CecClaim` on each `Unconfigured` event with backoff 1 s..30 s; wake attempts still transmitted from Unregistered, outcome `Unreachable` |
| Controller lizard state | treat as on: session drops reports until `lizard_off` confirmed by a feature-report read-back (#5)                                       |
| System slept            | the clock executor compares `CLOCK_BOOTTIME - CLOCK_MONOTONIC`; a jump > 2 s is `Sleep(Resumed)` even without the hook                   |

## Controller session (yakin-core, new module `session`)

Owns everything today split between `hidraw::run`'s setup closure, `Device.lizard/handed/leaks`,
`Engine::set_lizard`, `watch`, `hold` and `disconnect`.

```rust
pub enum Phase {
    /// Node open, controller may still answer EIO (Bluetooth settling).
    Settling { since: Instant, tries: u32 },
    /// lizard off and IMU on confirmed by read-back.
    Ready,
    /// Handed to its own keyboard/mouse by a `lizard:` binding.
    Handed,
    /// Power-off sent; reports after this are ignored until the link drops and returns.
    PoweringOff { since: Instant },
}
pub struct Session { phase: Phase, link: IpcLink, wanted: Setup /* lizard_off, imu */ }
impl Session {
    pub fn opened(link: IpcLink, wanted: Setup, now: Instant) -> (Self, Vec<Feature>);
    pub fn feature_result(&mut self, feature: Feature, result: FeatureResult, now: Instant) -> Vec<Feature>;
    pub fn report(&mut self, state: ControllerState) -> Option<ControllerState>; // None while not Ready
    pub fn lizard(&mut self, on: bool) -> Vec<Feature>;      // from a binding
    pub fn power_off(&mut self, now: Instant) -> Vec<Feature>;
    pub fn link(&mut self, state: LinkState, now: Instant) -> Vec<Feature>; // Wireless connect: Settling again
    pub fn tick(&mut self, now: Instant) -> Vec<Feature>;    // retry cadence
    pub fn is_ready(&self) -> bool;
}
pub enum Feature { LizardOff, LizardOn, ImuOn, PowerOff, ReadSetup }   // what the hidraw executor applies
pub enum FeatureResult { Ok, Busy /* EIO */, Gone /* EPIPE/ENODEV */, Err(String) }
```

Bug 5 dies here: a session leaves `Settling` only when `ReadSetup` confirms lizard is off (the
controller's own setting report), not when the write did not error. Every `Wireless::Connect`
puts the session back into `Settling`, so the "reconnect without a connect report" case is
handled by the generic path, not by `Engine::device` remembering `lizard == Some(true)`. The
grabbed-keyboard leak counter (`watch`) becomes a test oracle, not a repair mechanism.

## Executors (threads)

Each is a thread owning one resource, reading `Command`s on a channel and sending `Fact`s back.
Every command yields exactly one `Fact::Outcome`. None holds policy.

### `cec::Adapter` (yakin-core `device/cec.rs`, kernel CEC API via `nix::ioctl`)

Replaces `cec-ctl` shell hooks, `cec-tv-standby`, `cec-claim` and most of `cec-wake`. Same
flavour as `hidraw.rs`: open `/dev/cec0`, `CEC_ADAP_G_CAPS`, `CEC_ADAP_S_LOG_ADDRS`
(Playback, OSD "settop"), `poll` on the fd for `CEC_DQEVENT` (state changes: phys addr, log
addr mask) and `CEC_RECEIVE` (messages), `CEC_TRANSMIT` for the six opcodes we use:

| We send                                    | We listen for                                                           |
| ------------------------------------------ | ----------------------------------------------------------------------- |
| Give Device Power Status (0x8f)            | Report Power Status (0x90), also broadcasts not addressed to us         |
| Image View On (0x04), Active Source (0x82) | Standby (0x36) from TV: `Fact::TvPower(Standby, Broadcast)`             |
| Standby (0x36) to TV                       | Set Stream Path / Routing Change / Active Source: TV is on and switched |
|                                            | `CEC_EVENT_STATE_CHANGE`: `Fact::CecLink`                               |

Wake with `Link::Unconfigured`: if `CEC_CAP_PHYS_ADDR`, set phys addr 1.0.0.0 then claim; then
transmit Image View On from whatever address we have (Unregistered is legal for Image View On
per CEC 1.4 §13.13.5, and the Insignia honours it; the Samsung does not). Report `Unreachable`
when the transmit status is `CEC_TX_STATUS_NACK`/`ERROR` from Unregistered. The owner handles
the retry on `Configured`.

Fallback: `[daemon] tv = "shell"` keeps today's `tv_on`/`tv_off`/`tv_query` hooks as a
`shell::Executor` that maps exit code to `Outcome` and `TvQuery` success to `Fact::TvPower(On,
Query)`. The CEC executor is the default when `/dev/cec0` exists.

### `bluetooth` (existing module, becomes a thread)

`BluetoothDisconnect(id)` → `bluetoothctl disconnect <addr>` as now, absolute path from config
or `PATH` checked at start with a loud `error!` (#1's first half becomes a start-up failure,
not a runtime silence). Later: BlueZ over D-Bus with `zbus::blocking` if the shell-out proves
flaky; not now.

### `hidraw` (existing read thread, minus setup policy)

The read loop stops deciding retries. It applies `Feature`s it is sent, answering
`FeatureResult`, and forwards reports and link events. `Hidraw::run` shrinks to read/poll/apply.

### `sleep`

Two sources, same fact: a `systemd-sleep` hook in NixOS
(`environment.etc."systemd/system-sleep/yakin"`) calling `yakin-cli sleep pre|post` over the
existing IPC (new `IpcRequest::Sleep(SleepPhase)`), and the clock-jump detector inside the
engine's `tick`. `Preparing` is only known through the hook; `Resumed` through either. No
D-Bus client in the daemon: logind's `PrepareForSleep` is exactly what the hook relays, and the
hook also delays the suspend until the controller power-off has been sent (the hook blocks
while `yakin-cli sleep pre` waits for the `Outcome`s, bounded 2 s).

## Bug 6: input layer, the small part that moves

The gate's gesture logic (`hold`, `draining`) stays in `power::Tv`, but `tv:off` becomes a
plain `Output::Tv(TvIntent::Standby)` from the mapper like any binding, with `long_ms`/`warn_ms`
handled by the mapper's activator as today. `warn_ms`'s beep is `Command::Say(Cue::Warn)`,
decided in the mapper, executed by haptics. Wake-by-hold stays in the owner because it only
exists while gated.

Chord lists: `Profile::load` merges `[[profile.X.chord]]` entries by `hold` key when a later
file (override/drop-in) names the same `hold`, instead of appending a duplicate; document that
`hold = "quick"` twice is a merge. Virtual stick buttons (`lstick_up`) are already buttons to the
mapper after `faf8cd6`; the design adds nothing but a test row.

## Scenario tests (pure machines, fake `Instant`s)

Driver: `fn run(tv: &mut Tv, script: &[(Ms, Step)]) -> Vec<(Ms, Command)>` where `Step` is a
`Fact`, a report pair or a tick. Table rows are the tests; names are the test fns.

| #   | Scenario                                   | Script                                                                                     | Expected                                                                           |
| --- | ------------------------------------------ | ------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| 1a  | reconnect after standby does not wake      | Known(Standby); `ControllerLink(Connected)`                                                | no `TvWake`, gate still on                                                         |
| 1b  | BT disconnect failing is visible           | `tv:off`; `Outcome(BluetoothDisconnect)=Failed`                                            | `Pending(Standby)` still times out to Unknown; `Cue::Failed`                       |
| 2a  | remote turns TV on, heard                  | Known(Standby); `Fact::TvPower(On, Broadcast)`                                             | gate off, next report passes, no wake                                              |
| 2b  | remote turns TV on, not heard              | Known(Standby) at t0; tick at t0+5 min                                                     | `Unknown` + `TvQuery`; reports pass meanwhile                                      |
| 3a  | resume with CEC unconfigured               | `Sleep(Resumed)`; `CecLink(Unconfigured)`                                                  | `TvQuery` + `CecClaim`; input passes                                               |
| 3b  | wake while unconfigured                    | Known(Standby); hold 1 s; `Outcome(TvWake)=Unreachable`; `CecLink(Configured)`             | `TvWake` twice, second after Configured; `Pending(On)` until `TvPower(On)`         |
| 3c  | wake never confirmed                       | as 3b, no Configured, tick +3 s                                                            | `Unknown`, `TvQuery`, `Cue::Failed`, input passes                                  |
| 4   | suspend powers controllers down            | Known(On); `Sleep(Preparing)`                                                              | `ControllerOff`, `BluetoothDisconnect`, `TvStandby` (if `sleep_tv_off`), `Unknown` |
| 5a  | lizard confirmed before reports pass       | `Session::opened`; `feature_result(LizardOff, Busy)` ×3; `Ok`; `ReadSetup` says on         | still Settling, `LizardOff` re-sent; reports dropped                               |
| 5b  | wireless reconnect re-settles              | Ready; `link(Connected)`                                                                   | Settling, `LizardOff` + `ImuOn` emitted                                            |
| 5c  | `lizard:hold` round trip                   | Ready; `lizard(true)`; `lizard(false)`                                                     | Handed drops reports; back to Settling→Ready on confirmation                       |
| 6a  | tv:off warn then fire                      | menu held under grip chord: 2000 ms warn, 4000 ms fire; release at 3000 ms                 | `Say(Warn)` at 2 s; no `TvStandby`                                                 |
| 6b  | release inside the standby report goes out | key down; report with `[Key up, Tv(Standby)]`                                              | `Key up` passes, then `TvStandby`                                                  |
| 6c  | chord merge on override                    | two `[[chord]]` with `hold="quick"`                                                        | one chord, union of buttons, later wins per button                                 |
| 6d  | stick direction as chord button            | `grip_l2` held, `lstick_up` deflected                                                      | bound target fires, `lstick` axis target does not                                  |
| 7   | restart assumes nothing                    | `Tv::new(t0)`                                                                              | `Unknown`, `TvQuery` emitted; first report passes                                  |
| 8   | never hung                                 | any sequence: assert no state swallows past `max(tv_wake, 3 s)` without a hold in progress | property test over random scripts, 10k steps                                       |

Fake clock: all `Instant`s are `t0 + Duration`; no `sleep`. Fake TV: a tiny `struct FakeTv {
power: TvPower, hpd: bool }` in tests that turns `Command`s into the `Fact`s a real Samsung or
Insignia would send (two tables), so rows 2–3 run against both.

## Migration: atomic, shippable steps, deadliest bug first

Each step is one commit on `history`, builds, keeps the settop usable, and deletes something.

1. **`Belief<T>` + `power::Tv` with `Unknown` start and horizon decay, shell executor as is.** Replace `tv::Tv`; `query_tv` becomes `Command::TvQuery` on `Unknown` entry and at `tv_belief_s`. Kills #2, #7, #8 (the sticky Off is gone). Tests: rows 2b, 7, 8.
2. **Outcomes.** `power()`/`query_tv()` threads report `Fact::Outcome`; `Pending` with a 3 s timeout; connect emits `TvActiveSource` only. Kills #1 for good (connect can no longer wake; a failed disconnect is a logged outcome and a cue). Rows 1a, 1b.
3. **Sleep facts.** `IpcRequest::Sleep`, the NixOS `system-sleep` hook, the clock-jump detector. Controllers powered down before suspend; belief reset on resume. Kills #4, half of #3. Row 4, 3a.
4. **`cec::Adapter` executor** behind `[daemon] tv = "cec"`: claim, state-change events, the six opcodes, `Unreachable`. The NixOS units `cec-tv-standby`, `cec-claim` and the polling in `cec-wake` are removed; `cec-tv` (boot/shutdown) stays. Kills #3, and makes #2 instant. Rows 3b, 3c, 2a.
5. **`session::Session`.** Setup policy out of `hidraw::run`; `ReadSetup` confirmation; `Device.lizard/handed/leaks` and `Engine::watch` deleted. Kills #5. Rows 5a–5c.
6. **Input nits.** `Output::Tv(TvIntent)`, `Cue::Warn` from the mapper, chord merge on override. Row 6a–6d. Closes #6.
7. **Engine diet.** With 1–6 in, `Engine::device` is a dispatch; split `engine.rs` into `engine/{devices,ipc,menu}.rs` without behaviour change.

Steps 1–3 need no NixOS change except the hook in 3. Step 4 changes `cec.nix`; ship the daemon
first with `tv = "shell"` default, flip the default once the Samsung and the Insignia both pass
rows 2a/3b on the real box.

## What NOT to build

- A general CEC stack (libcec, `cec-rs`, remote-key handling). Six opcodes, one logical address. The remote's keys stay with the kernel's `rc-cec` and `cec-vendor-keys`.
- A D-Bus client in the daemon (logind, BlueZ). The sleep hook and `bluetoothctl` are enough; revisit only if they prove flaky.
- Persisting belief across restarts. `Unknown` + one query at start is cheaper and always right.
- A generic event-sourcing/actor framework. Three pure machines and five threads with channels, as the code is already shaped.
- Fixing niri's 640x480 after resume inside yakin. That is an output rule (`output "HDMI-A-1" { mode "3840x2160@60" }`) or a kernel `video=` argument in nixserver; the design only makes sure yakin does not depend on it.
- Timing-based "is the TV on" heuristics (HDMI sink state, audio sink presence). CEC reports or Unknown.

## Open questions for Cal

1. **CEC in-process (kernel API via `nix` ioctls) or keep shelling to `cec-ctl`?** Recommend in-process (step 4): it is the only way to hear the TV's broadcasts, which turns #2 and #3 from polling into events, and it mirrors `hidraw.rs`. ~400 lines, no new crate.
2. **TV to standby on suspend?** Recommend yes by default (`sleep_tv_off = true`): the settop leaving is the same as `tv:off`, and it avoids a TV showing "no signal" for 30 minutes.
3. **Belief horizon.** Recommend 5 min for `Known(Standby)` and never for `Known(On)` under CEC (the TV tells us when it leaves); 5 min for both under the shell executor.
4. **Wake while unconfigured on the Samsung fails for good?** If the Samsung truly ignores Image View On from Unregistered and drops HPD in standby, the only remaining wake path is the TV's own remote or Wake-on-CEC via `User Control Pressed Power` (also Unregistered). Tested 2026-11-07: the HDMI card's adapter ignores a manual `cec-ctl -p 1.0.0.0` (its physical address comes from the DRM connector only) and the Samsung ignored Image View On from Unregistered. Settled: no CEC wake while the Samsung has HPD down; `Cue::Failed`, and the suspend policy must not rely on it (network wake via Home Assistant is the open alternative).

## Implementation notes

Step 1 (`yakin-core/src/power.rs`, replacing `tv.rs`):

- `Belief` has `Unknown` and `Known` only, `Source` has `Query` and `Hook`, `TvPower` has `On`
  and `Standby`, `Fact` has `TvPower`: each later step adds the variants it uses.
- `Known(Standby)` is re-asked at the horizon but **not dropped**. A shell `tv_query` can only
  prove the TV on (grep exits 1 for both "standby" and "cec-ctl failed"), so dropping the gate
  would make wake-by-hold stop working five minutes after `tv:off`. `Known(On)` and `Unknown`
  behave as the table says (On decays to Unknown with a query; Unknown re-asks each horizon).
  Row 2b therefore reads: standby at t0, tick at +5 min gives `TvQuery`, an `On` answer frees
  the gate; silence keeps it. Step 4 can drop it with real standby reports.
- The horizon exists only with a `tv_query` configured (`tv_belief_s`, default 300, 0 = never).
- `Tv::new` returns the initial `TvQuery` with the value. `ControllerLink` is
  `Tv::controller_connected()`: on connect with `Known(On)` it returns `TvWake` (the `tv_on`
  hook is also the active-source hook in shell mode), with `Known(Standby)` a `TvQuery`.
- `tv:off` gives `[ControllersOff, TvStandby]`: one `ControllersOff` for every controller
  instead of per-id `ControllerOff` + `BluetoothDisconnect`, as the owner does not know ids.
- The property test (row 8) asserts the liveness that holds: from any state reached by 10k
  random steps, a release and one held button wake or free the gate, and the next press passes.

Step 2:

- `Fact::Outcome` names the `Command` it answers instead of a `CommandId`: one command of each
  kind is in flight at most, so the kind is enough. `Outcome` has `Done` and `Failed`
  (`Unreachable` comes with the CEC executor); `Cue` has `Failed`, a 300 ms tone on the right
  pad of every controller.
- Under the shell executor a command that exits 0 is taken as seen (`Pending` -> `Known(_,
  Hook)`); an unset hook counts as done. `Pending` times out after a fixed 3 s (`PENDING`), so a
  `tv_on` script slower than that reads as a failure; the late outcome is ignored.
- The BluetoothDisconnect outcome is `Outcome(ControllersOff, Failed)`, reported when
  `bluetoothctl` fails; a failed `power_off` write on a node that is gone is only logged.
- `TvQuery` reports nothing back besides the `On` fact: silence or a non-zero exit changes no
  belief.

Step 3:

- `Fact::Sleep(SleepPhase)`; `SleepPhase` lives in `power.rs` (the IPC message needs it) and
  parses `pre`/`post` for `yakin-cli sleep`. `Preparing` forgets the TV without querying (the
  machine is going away) and returns `[ControllersOff, TvStandby]`, the standby only with
  `sleep_tv_off` (config, default true) and not when a standby is already believed or wanted.
  `Resumed` returns `TvQuery`.
- The clock-jump detector is `yakin_core::sleep::Sleeps`, a pure value fed `CLOCK_BOOTTIME`
  and `CLOCK_MONOTONIC` by the engine's two-second tick; a jump over 2 s is `Resumed`. It does
  not dedupe against the hook: both may arrive, which asks the TV twice.
- `yakin-cli sleep pre` is answered once every thread the suspend started has finished (each
  holds an end of a channel), or after 2 s, so the hook delays the suspend by that long at most.
- The hook is `nixos/yakin-sleep.nix` in nixserver (`systemd.services` is not enough: a
  `system-sleep` script runs after the units are frozen, so it is an `environment.etc` entry).
