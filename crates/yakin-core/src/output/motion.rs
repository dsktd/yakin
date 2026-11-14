//! Raw motion on a Unix socket, for game runtimes that read true motion themselves (NX2navi):
//! `YAKMOT1\0` on accept, then one [`RECORD_LEN`]-byte record per IMU report. See DESIGN.md
//! section 5, "Motion socket".

use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;

use crossbeam_channel::Receiver;
use log::debug;
use log::warn;
use nix::time::ClockId;

use crate::Result;
use crate::state::Imu;

pub const MAGIC: &[u8; 8] = b"YAKMOT1\0";
pub const RECORD_LEN: usize = 32;
const STANDARD_GRAVITY: f32 = 9.80665;
/// `STEAM_ACCEL_RES_PER_G` and `STEAM_GYRO_RES_PER_DPS` in hid-steam.
const ACCEL_UNITS_PER_G: f32 = 16384.0;
const GYRO_UNITS_PER_DPS: f32 = 16.0;

/// One IMU report in SDL3's gamepad sensor frame: +x right, +y up out of the face, +z toward
/// the player; accel in m/s² (flat at rest about (0, +9.81, 0)), gyro in rad/s about those
/// axes, right-handed (x pitch, y yaw, z roll).
///
/// The report holds X, Z, Y (bytes 34-38, 40-44). The mapping is hid-steam's
/// `steam_ibex_imu_mappings` (7.3): ABS_X = +34, ABS_Y = +38, ABS_Z = -36 and likewise for the
/// gyro, whose evdev axes SDL reads as its sensor axes unchanged. Sure of y (a controller at
/// rest reads +16291 at byte 38); x, z and the gyro signs rest on the kernel's table only.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Sample {
    /// CLOCK_MONOTONIC when the report was handled.
    pub micros: u64,
    pub accel: [f32; 3],
    pub gyro: [f32; 3],
}
impl Sample {
    pub fn new(
        micros: u64,
        imu: Imu,
    ) -> Self {
        let [x, z, y] = imu.accel;
        let accel = |raw: i16| raw as f32 / ACCEL_UNITS_PER_G * STANDARD_GRAVITY;
        let [pitch, roll, yaw] = imu.gyro;
        let gyro = |raw: i16| (raw as f32 / GYRO_UNITS_PER_DPS).to_radians();
        Self {
            micros,
            accel: [accel(x), accel(y), -accel(z)],
            gyro: [gyro(pitch), gyro(yaw), -gyro(roll)],
        }
    }

    /// The sample as the profile's `gyro.hold` lets it through: released, the gyro reads 0
    /// and the accel is kept, so aim integrated from the gyro ratchets and tilt does not.
    pub fn held(
        mut self,
        held: bool,
    ) -> Self {
        if !held {
            self.gyro = [0.0; 3];
        }
        self
    }

    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut record = [0; RECORD_LEN];
        record[..8].copy_from_slice(&self.micros.to_le_bytes());
        let floats = self.accel.iter().chain(&self.gyro);
        for (chunk, value) in record[8..].as_chunks_mut::<4>().0.iter_mut().zip(floats) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        record
    }

    pub fn decode(record: &[u8; RECORD_LEN]) -> Self {
        let float = |at: usize| f32::from_le_bytes(record[at..at + 4].try_into().unwrap());
        Self {
            micros: u64::from_le_bytes(record[..8].try_into().unwrap()),
            accel: [float(8), float(12), float(16)],
            gyro: [float(20), float(24), float(28)],
        }
    }
}

/// Microseconds on CLOCK_MONOTONIC, the clock of [`Sample::micros`].
pub fn monotonic_micros() -> u64 {
    match nix::time::clock_gettime(ClockId::CLOCK_MONOTONIC) {
        Ok(now) => now.tv_sec() as u64 * 1_000_000 + now.tv_nsec() as u64 / 1000,
        Err(_) => 0,
    }
}

/// The listening socket and its read-only clients. Sending never blocks: a client that
/// cannot take a whole record at once is dropped.
pub struct MotionSocket {
    path: PathBuf,
    rx_client: Receiver<UnixStream>,
    clients: Vec<UnixStream>,
}
impl MotionSocket {
    /// Listen on `path` (mode 0600, its directory made 0700), replacing a stale socket. A
    /// thread accepts and greets clients.
    pub fn bind(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let (tx_client, rx_client) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let greeted = stream.and_then(|mut stream| {
                    stream.write_all(MAGIC)?;
                    stream.set_nonblocking(true)?;
                    Ok(stream)
                });
                match greeted {
                    Ok(stream) => {
                        if tx_client.send(stream).is_err() {
                            return;
                        }
                    }
                    Err(err) => warn!("Motion client: {err}"),
                }
            }
        });
        Ok(Self {
            path: path.to_path_buf(),
            rx_client,
            clients: Vec::new(),
        })
    }

    pub fn send(
        &mut self,
        sample: Sample,
    ) {
        self.clients.extend(self.rx_client.try_iter());
        let record = sample.encode();
        self.clients
            .retain_mut(|client| match client.write(&record) {
                Ok(RECORD_LEN) => true,
                written => {
                    debug!("Motion client dropped: {written:?}");
                    false
                }
            });
    }
}
impl Drop for MotionSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod test {
    use std::io::Read;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    fn imu(
        accel: [i16; 3],
        gyro: [i16; 3],
    ) -> Imu {
        Imu {
            accel,
            gyro,
            ..Imu::default()
        }
    }

    #[test]
    fn test_axes() {
        // The BLE fixture at rest: report order X, Z, Y
        let sample = Sample::new(0, imu([-979, -1818, 16291], [0; 3]));
        assert!((sample.accel[1] - 9.75).abs() < 0.01, "{sample:?}");
        assert!((sample.accel[0] + 0.586).abs() < 0.01, "{sample:?}");
        assert!((sample.accel[2] - 1.088).abs() < 0.01, "{sample:?}");
        // 16 units = 1 °/s: pitch X as is, yaw from Y, roll from -Z
        let sample = Sample::new(0, imu([0; 3], [16, 32, 48]));
        let rad = 1.0_f32.to_radians();
        assert!((sample.gyro[0] - rad).abs() < 1e-6, "{sample:?}");
        assert!((sample.gyro[1] - 3.0 * rad).abs() < 1e-6, "{sample:?}");
        assert!((sample.gyro[2] + 2.0 * rad).abs() < 1e-6, "{sample:?}");
    }

    #[test]
    fn test_held() {
        let sample = Sample::new(7, imu([-979, -1818, 16291], [160, 320, -480]));
        assert_eq!(sample.held(true), sample);
        let released = sample.held(false);
        assert_eq!(released.gyro, [0.0; 3]);
        assert_eq!(released.accel, sample.accel);
        assert_eq!(released.micros, 7);
    }

    #[test]
    fn test_encode() {
        let sample = Sample {
            micros: 0x0102_0304_0506_0708,
            accel: [1.0, -2.0, 9.5],
            gyro: [0.25, 0.0, -0.5],
        };
        let record = sample.encode();
        assert_eq!(record[..8], [8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(record[8..12], 1.0_f32.to_le_bytes());
        assert_eq!(record[28..], (-0.5_f32).to_le_bytes());
        assert_eq!(Sample::decode(&record), sample);
    }

    fn socket(name: &str) -> (PathBuf, MotionSocket) {
        let dir = std::env::temp_dir().join(format!("yakin-motion-{name}-{}", std::process::id()));
        let path = dir.join("motion.sock");
        let socket = MotionSocket::bind(&path).unwrap();
        (path, socket)
    }

    /// Send until the accept thread has handed the client over.
    fn send_until_clients(
        socket: &mut MotionSocket,
        count: usize,
    ) {
        let start = Instant::now();
        while socket.clients.len() < count {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
            socket.clients.extend(socket.rx_client.try_iter());
        }
    }

    #[test]
    fn test_client() {
        let (path, mut socket) = socket("client");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let mut client = UnixStream::connect(&path).unwrap();
        send_until_clients(&mut socket, 1);
        let sample = Sample::new(42, imu([1, 2, 3], [4, 5, 6]));
        socket.send(sample);
        let mut magic = [0; 8];
        client.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, MAGIC);
        let mut record = [0; RECORD_LEN];
        client.read_exact(&mut record).unwrap();
        assert_eq!(Sample::decode(&record), sample);
        drop(socket);
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn test_stalled_client() {
        let (path, mut socket) = socket("stalled");
        // Never read from
        let _stalled = UnixStream::connect(&path).unwrap();
        let mut reader = UnixStream::connect(&path).unwrap();
        send_until_clients(&mut socket, 2);
        reader.set_nonblocking(true).unwrap();
        let start = Instant::now();
        let mut buf = vec![0; 1 << 16];
        while socket.clients.len() == 2 {
            assert!(start.elapsed() < Duration::from_secs(5), "never dropped");
            socket.send(Sample::new(0, Imu::default()));
            let _ = reader.read(&mut buf);
        }
        // The reader is the one kept, and sending did not block
        assert_eq!(socket.clients.len(), 1);
        drop(socket);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
