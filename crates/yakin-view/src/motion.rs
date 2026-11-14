//! The model turns as the controller does: gyro rates integrated, pulled toward gravity so
//! pitch and roll do not drift. Yaw has no reference: it starts at the controller's heading
//! when following turns on and stays relative to that.

use std::f32::consts::FRAC_PI_2;

use bevy::prelude::*;
use yakin_core::output::motion::Sample;
use yakin_core::profile::GyroHold;
use yakin_core::state::Imu;

use crate::model::Body;
use crate::model::Input;
use crate::model::Selection;

pub fn plugin(app: &mut App) {
    app.init_resource::<Motion>();
    app.add_systems(Update, tilt);
}

/// Fraction a second the estimate is pulled toward the accelerometer's gravity.
const GRAVITY_GAIN: f32 = 2.0;
/// Rates under this are still, rad/s (1 °/s, as the daemon's gyro).
const STILL: f32 = 0.017;
/// Accel this far from 1 g is a shake, not gravity, and is left out.
const SHAKE: f32 = 0.2;
/// A longer gap between frames (a sleeping scene) is taken as this long.
const LONGEST_STEP: f32 = 0.1;
/// How long the model takes to ease to its target after a hold or a toggle.
pub const EASE_SECONDS: f32 = 0.4;
const STANDARD_GRAVITY: f32 = 9.80665;

/// The controller's orientation in SDL's sensor frame (+y up out of the face, +z toward the
/// player), as the rotation from lying flat, face up, to now.
#[derive(Clone, Debug, Default)]
pub struct Fusion {
    pose: Option<Quat>,
}
impl Fusion {
    /// Integrate `gyro` (rad/s) over `seconds` and pull toward the gravity in `accel` (m/s²).
    /// The first sample starts from gravity alone.
    pub fn update(
        &mut self,
        accel: Vec3,
        gyro: Vec3,
        seconds: f32,
    ) -> Quat {
        let gravity = (accel.length() / STANDARD_GRAVITY - 1.0).abs() < SHAKE;
        let Some(pose) = self.pose else {
            let pose = if gravity {
                Quat::from_rotation_arc(accel.normalize(), Vec3::Y)
            } else {
                Quat::IDENTITY
            };
            self.pose = Some(pose);
            return pose;
        };
        let gyro = if gyro.length() < STILL {
            Vec3::ZERO
        } else {
            gyro
        };
        let mut pose = (pose * Quat::from_scaled_axis(gyro * seconds)).normalize();
        if gravity {
            // The least turn that lines the measured up with the world's: pitch and roll only
            let up = pose * accel.normalize();
            let pull = Quat::from_rotation_arc(up, Vec3::Y);
            let k = (GRAVITY_GAIN * seconds).min(1.0);
            pose = (Quat::IDENTITY.slerp(pull, k) * pose).normalize();
        }
        self.pose = Some(pose);
        pose
    }

    /// Take the current heading as straight ahead, keeping pitch and roll.
    pub fn recentre(&mut self) {
        let Some(pose) = self.pose else {
            return;
        };
        let twist = Quat::from_xyzw(0.0, pose.y, 0.0, pose.w);
        if twist.length_squared() > f32::EPSILON {
            self.pose = Some((twist.normalize().inverse() * pose).normalize());
        }
    }

    pub fn pose(&self) -> Quat {
        self.pose.unwrap_or(Quat::IDENTITY)
    }
}

/// A rotation in SDL's sensor frame turned into the model's (+y the far edge, +z out of the
/// face): lying flat in front of you is the model's default pose, seen from where your eyes are.
pub fn to_model(pose: Quat) -> Quat {
    let frame = Quat::from_rotation_x(FRAC_PI_2);
    frame * pose * frame.inverse()
}

/// The accel and gyro of a report in SDL's frame. The axis signs are the motion socket's
/// (`yakin_core::output::motion::Sample`), so a flipped sign is fixed there for both. `None`
/// with the IMU off: the report's accel is all zero.
pub fn sensors(imu: Imu) -> Option<(Vec3, Vec3)> {
    if imu.accel == [0; 3] {
        return None;
    }
    let sample = Sample::new(0, imu);
    Some((Vec3::from(sample.accel), Vec3::from(sample.gyro)))
}

/// The pose shown: the target as it is, held still while a control is picked, and eased back
/// in after a hold or a toggle so it never snaps.
#[derive(Clone, Debug)]
pub struct Follow {
    shown: Quat,
    /// Where an ease started and how far along it is, 0 to 1.
    ease: Option<(Quat, f32)>,
}
impl Default for Follow {
    fn default() -> Self {
        Self {
            shown: Quat::IDENTITY,
            ease: None,
        }
    }
}
impl Follow {
    /// Ease from what is shown to the next targets.
    pub fn ease(&mut self) {
        self.ease = Some((self.shown, 0.0));
    }

    pub fn step(
        &mut self,
        target: Quat,
        held: bool,
        seconds: f32,
    ) -> Quat {
        if held {
            self.ease();
            return self.shown;
        }
        self.shown = match &mut self.ease {
            None => target,
            Some((from, t)) => {
                *t = (*t + seconds / EASE_SECONDS).min(1.0);
                let k = *t * *t * (3.0 - 2.0 * *t);
                let shown = from.slerp(target, k);
                if *t >= 1.0 {
                    self.ease = None;
                }
                shown
            }
        };
        self.shown
    }
}

/// Whether the model follows the controller, and the state that takes.
#[derive(Resource, Clone, Debug, Default)]
pub struct Motion {
    on: bool,
    fusion: Fusion,
    follow: Follow,
    /// Showing what a game aiming with the gyro sees: the gyro integrated only while engaged.
    ratchet: bool,
    aim: Quat,
}
impl Motion {
    /// Turn following on or off. On takes the controller's heading as straight ahead; either
    /// way the model eases over.
    pub fn set(
        &mut self,
        on: bool,
    ) {
        if on == self.on {
            return;
        }
        self.on = on;
        if on {
            self.fusion.recentre();
        }
        self.aim = self.fusion.pose();
        self.follow.ease();
    }

    /// Show the ratcheted aim instead of the real pose, starting from the real one; either way
    /// the model eases over.
    pub fn set_ratchet(
        &mut self,
        on: bool,
    ) {
        if on == self.ratchet {
            return;
        }
        self.ratchet = on;
        self.aim = self.fusion.pose();
        self.follow.ease();
    }

    /// The pose to show after `seconds` of `imu`, held while `held`; the aim turns only while
    /// the gyro is `engaged`, with no pull toward gravity, as a game's gyro aim does.
    pub fn step(
        &mut self,
        imu: Imu,
        engaged: bool,
        held: bool,
        seconds: f32,
    ) -> Quat {
        let seconds = seconds.min(LONGEST_STEP);
        let sensors = sensors(imu);
        if let Some((accel, gyro)) = sensors {
            let first = self.fusion.pose.is_none();
            self.fusion.update(accel, gyro, seconds);
            if first {
                self.aim = self.fusion.pose();
            } else if engaged && gyro.length() >= STILL {
                self.aim = (self.aim * Quat::from_scaled_axis(gyro * seconds)).normalize();
            }
        }
        let pose = match self.ratchet {
            true => self.aim,
            false => self.fusion.pose(),
        };
        let target = match (self.on, sensors) {
            (true, Some(_)) => to_model(pose),
            _ => Quat::IDENTITY,
        };
        self.follow.step(target, held && self.on, seconds)
    }
}

fn tilt(
    input: Res<Input>,
    selection: Res<Selection>,
    time: Res<Time>,
    mut motion: ResMut<Motion>,
    mut bodies: Query<&mut Transform, With<Body>>,
) -> Result<()> {
    let mut transform = bodies.single_mut()?;
    // The view knows no profile: the default `gyro.hold`, either grip
    let engaged = GyroHold::Grip.held(input.0.buttons);
    let pose = motion.step(
        input.0.imu,
        engaged,
        selection.0.is_some(),
        time.delta_secs(),
    );
    if transform.rotation != pose {
        transform.rotation = pose;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    const G: f32 = STANDARD_GRAVITY;

    fn close(
        a: Quat,
        b: Quat,
    ) -> bool {
        a.angle_between(b) < 1e-3
    }

    #[test]
    fn test_rest_from_gravity() {
        let mut fusion = Fusion::default();
        let flat = fusion.update(Vec3::new(0.0, G, 0.0), Vec3::ZERO, 0.01);
        assert!(
            close(flat, Quat::IDENTITY),
            "flat, face up is the rest pose"
        );
        let mut fusion = Fusion::default();
        // Standing on its bottom edge, face toward the player: gravity along +z
        let up = fusion.update(Vec3::new(0.0, 0.0, G), Vec3::ZERO, 0.01);
        assert!((up * Vec3::Z).abs_diff_eq(Vec3::Y, 1e-4));
        assert!(close(to_model(Quat::IDENTITY), Quat::IDENTITY));
    }

    #[test]
    fn test_rest_holds_still() {
        let mut fusion = Fusion::default();
        let accel = Vec3::new(0.0, G, 0.0);
        fusion.update(accel, Vec3::ZERO, 0.01);
        for _ in 0..1000 {
            // Noise under the still threshold
            fusion.update(accel, Vec3::new(0.01, -0.01, 0.005), 0.01);
        }
        assert!(close(fusion.pose(), Quat::IDENTITY));
    }

    #[test]
    fn test_known_rotation() {
        let mut fusion = Fusion::default();
        fusion.update(Vec3::new(0.0, G, 0.0), Vec3::ZERO, 0.01);
        // A quarter turn of yaw over a second; gravity stays along +y, so it cannot pull
        let rate = FRAC_PI_2;
        for _ in 0..100 {
            fusion.update(Vec3::new(0.0, G, 0.0), Vec3::new(0.0, rate, 0.0), 0.01);
        }
        assert!(
            close(fusion.pose(), Quat::from_rotation_y(FRAC_PI_2)),
            "{:?}",
            fusion.pose()
        );
    }

    #[test]
    fn test_gravity_pulls_drift() {
        let mut fusion = Fusion::default();
        let accel = Vec3::new(0.0, G, 0.0);
        fusion.update(accel, Vec3::ZERO, 0.01);
        // A pitch the accelerometer never saw: the estimate drifts back to flat
        for _ in 0..25 {
            fusion.update(accel, Vec3::new(0.5, 0.0, 0.0), 0.02);
        }
        assert!(fusion.pose().angle_between(Quat::IDENTITY) > 0.05);
        for _ in 0..500 {
            fusion.update(accel, Vec3::ZERO, 0.01);
        }
        assert!(fusion.pose().angle_between(Quat::IDENTITY) < 0.01);
    }

    #[test]
    fn test_recentre() {
        let mut fusion = Fusion::default();
        fusion.update(Vec3::new(0.0, G, 0.0), Vec3::ZERO, 0.01);
        for _ in 0..50 {
            fusion.update(Vec3::new(0.0, G, 0.0), Vec3::new(0.0, 1.0, 0.0), 0.01);
        }
        assert!(!close(fusion.pose(), Quat::IDENTITY));
        fusion.recentre();
        assert!(close(fusion.pose(), Quat::IDENTITY), "yaw gone");
        // Pitch stays
        let mut fusion = Fusion::default();
        let tilted = Quat::from_rotation_y(1.0) * Quat::from_rotation_x(0.5);
        fusion.pose = Some(tilted);
        fusion.recentre();
        let forward = fusion.pose() * Vec3::NEG_Z;
        assert!(forward.x.abs() < 1e-4, "facing straight ahead: {forward}");
        assert!((fusion.pose() * Vec3::Y).y < 0.99, "still pitched");
    }

    #[test]
    fn test_sensors_off() {
        assert_eq!(sensors(Imu::default()), None);
        let imu = Imu {
            accel: [0, 0, 16384],
            ..default()
        };
        let (accel, _) = sensors(imu).unwrap();
        assert!(accel.abs_diff_eq(Vec3::new(0.0, G, 0.0), 1e-3));
    }

    #[test]
    fn test_held_stays_then_eases() {
        let mut follow = Follow::default();
        let turned = Quat::from_rotation_y(1.0);
        assert!(close(follow.step(turned, false, 0.016), turned), "no lag");
        let other = Quat::from_rotation_x(0.7);
        assert!(
            close(follow.step(other, true, 0.016), turned),
            "a picked control holds the model"
        );
        assert!(close(follow.step(other, true, 1.0), turned));
        let first = follow.step(other, false, 0.016);
        assert!(!close(first, other), "released: no snap");
        assert!(
            first.angle_between(turned) < 0.01,
            "eases out from the hold"
        );
        let done = follow.step(other, false, EASE_SECONDS);
        assert!(close(done, other), "caught up");
    }

    #[test]
    fn test_ratchet() {
        let flat = Imu {
            accel: [0, 0, 16384],
            ..default()
        };
        let yawing = Imu {
            gyro: [0, 0, 16 * 90],
            ..flat
        };
        let mut motion = Motion::default();
        motion.step(flat, true, false, 0.016);
        motion.set(true);
        motion.set_ratchet(true);
        motion.follow.ease = None;
        // Engaged: aim and real pose turn together
        motion.step(yawing, true, false, 0.1);
        assert!(close(motion.aim, motion.fusion.pose()));
        let aim = motion.aim;
        // Released: the real pose turns on, the aim and the model stay
        let shown = motion.step(flat, true, false, 0.016);
        for _ in 0..5 {
            assert!(close(motion.step(yawing, false, false, 0.1), shown));
        }
        assert!(close(motion.aim, aim));
        assert!(!close(motion.fusion.pose(), aim));
        // Re-grabbed: turns on from where it was, no jump to the real pose
        let next = motion.step(yawing, true, false, 0.016);
        assert!(next.angle_between(shown) < 0.05, "no jump");
        assert!(!close(motion.aim, motion.fusion.pose()));
        // Off: the real pose again
        motion.set_ratchet(false);
        motion.follow.ease = None;
        let real = motion.step(flat, false, false, 0.016);
        assert!(close(real, to_model(motion.fusion.pose())));
    }

    #[test]
    fn test_toggle() {
        let flat_turned = Imu {
            accel: [0, 0, 16384],
            gyro: [0, 0, 0],
            ..default()
        };
        let mut motion = Motion::default();
        assert!(
            close(motion.step(flat_turned, true, false, 0.016), Quat::IDENTITY),
            "off: the default pose"
        );
        // Yaw while off still feeds the estimate
        let yawing = Imu {
            gyro: [0, 0, 16 * 90],
            ..flat_turned
        };
        motion.step(yawing, true, false, 0.1);
        assert!(!close(motion.fusion.pose(), Quat::IDENTITY));
        motion.set(true);
        assert!(
            close(motion.fusion.pose(), Quat::IDENTITY),
            "on: re-centred on the heading"
        );
        // Tilted on: eases over, then follows
        let tilted = Imu {
            accel: [0, 11585, 11585],
            ..flat_turned
        };
        motion.fusion.pose = Some(Quat::from_rotation_x(0.6));
        motion.follow.ease();
        let first = motion.step(tilted, true, false, 0.016);
        assert!(first.angle_between(Quat::IDENTITY) < 0.05, "no snap");
        let mut after = first;
        for _ in 0..30 {
            after = motion.step(tilted, true, false, 0.016);
        }
        assert!(after.angle_between(Quat::IDENTITY) > 0.3, "follows");
        motion.set(false);
        for _ in 0..30 {
            motion.step(tilted, true, false, 0.016);
        }
        assert!(close(
            motion.step(tilted, true, false, 0.016),
            Quat::IDENTITY
        ));
        assert!(
            close(
                motion.step(Imu::default(), true, false, 0.016),
                Quat::IDENTITY
            ),
            "no IMU: the default pose"
        );
    }
}
