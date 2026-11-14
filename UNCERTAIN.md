# Uncertain

Behaviour that ships unproven, optional, until verified on hardware.

- Motion socket (`daemon.motion_socket`, off by default): axis signs other than accel y, and
  the timestamp being receipt time in the engine, not the IMU's own clock. DESIGN.md section 5.
- 3D view follows the controller (on by default, "Follow the controller" switch on the 3D tab):
  the gyro and accel signs other than accel y (shared with the motion socket's mapping), the
  gravity pull and still threshold's feel. DESIGN.md, the 3D page.
- Gyro grip ratchet (`gyro.hold = "grip"`, on by default; `"none"` turns it off): the feel of
  gyro stopping when both grips are released, on the cursor, a gyro stick, the motion socket
  and the 3D tab's "As gyro aim" switch. DESIGN.md sections 4 and 5.
