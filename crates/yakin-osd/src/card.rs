//! The status card (`yakin-osd --status`): the time, the active profile, each controller's
//! battery, what is playing and the sleep timer, as one toast.

use std::time::Duration;

use yakin_ipc::IpcStatus;
use yakin_ipc::mpris::Player;

use crate::toast::Toast;

/// The card: `time` as the title, the profile under it, then batteries and what plays. With no
/// daemon to ask, `status` is none and the card says so; `sleep` is what the sleep timer has left.
pub fn card(
    time: &str,
    status: Option<&IpcStatus>,
    players: &[Player],
    sleep: Option<Duration>,
) -> Toast {
    let mut lines = Vec::new();
    let profile = match status {
        Some(status) => format!("Profile {}", status.profile),
        None => "yakin is not running".to_string(),
    };
    for controller in status
        .iter()
        .flat_map(|status| &status.controllers)
        .filter(|controller| controller.connected)
    {
        let battery = match controller.battery {
            Some(level) => format!("{level}%"),
            None => "?".to_string(),
        };
        lines.push(format!("Controller battery {battery}"));
    }
    if let Some(playing) = playing(players) {
        lines.push(playing);
    }
    if let Some(left) = sleep {
        lines.push(format!("Sleep in {} min", left.as_secs().div_ceil(60)));
    }
    let mut toast = Toast::new(time, Some(&profile));
    toast.lines = lines;
    toast
}

/// What the first playing player plays, else the first paused one.
fn playing(players: &[Player]) -> Option<String> {
    let player = players
        .iter()
        .find(|player| player.status == "Playing")
        .or(players.iter().find(|player| player.status == "Paused"))?;
    let sign = match player.status.as_str() {
        "Playing" => "Playing",
        _ => "Paused",
    };
    match player.artists.is_empty() {
        true => Some(format!("{sign} {}", player.title)),
        false => Some(format!(
            "{sign} {} – {}",
            player.artists.join(", "),
            player.title
        )),
    }
}

/// The local time, `21:04`.
pub fn now() -> String {
    jiff::Zoned::now().strftime("%H:%M").to_string()
}

#[cfg(test)]
mod test {
    use yakin_ipc::IpcController;

    use super::*;

    fn player(
        status: &str,
        title: &str,
        artists: &[&str],
    ) -> Player {
        Player {
            bus: String::new(),
            status: status.to_string(),
            title: title.to_string(),
            artists: artists.iter().map(|artist| artist.to_string()).collect(),
        }
    }

    fn controller(
        connected: bool,
        battery: Option<u8>,
    ) -> IpcController {
        IpcController {
            id: 0,
            path: "/dev/hidraw0".to_string(),
            battery,
            wireless: true,
            link: yakin_ipc::IpcLink::Puck,
            connected,
            lizard: Some(false),
            leaks: 0,
            mouse: false,
        }
    }

    #[test]
    fn test_card_lines() {
        let status = IpcStatus {
            profile: "desktop".to_string(),
            controllers: vec![controller(true, Some(80)), controller(false, Some(10))],
            ..IpcStatus::default()
        };
        let players = [
            player("Paused", "Old", &[]),
            player("Playing", "Song", &["Band"]),
        ];
        let sleep = Some(Duration::from_secs(41 * 60 + 5));
        let toast = card("21:04", Some(&status), &players, sleep);
        assert_eq!(toast.title, "21:04");
        assert_eq!(toast.subtitle.as_deref(), Some("Profile desktop"));
        assert_eq!(
            toast.lines,
            [
                "Controller battery 80%",
                "Playing Band – Song",
                "Sleep in 42 min"
            ]
        );
    }

    #[test]
    fn test_card_without_daemon_or_player() {
        let toast = card("07:00", None, &[player("Stopped", "Gone", &[])], None);
        assert_eq!(toast.subtitle.as_deref(), Some("yakin is not running"));
        assert!(toast.lines.is_empty());
        let paused = card("07:00", None, &[player("Paused", "Talk", &[])], None);
        assert_eq!(paused.lines, ["Paused Talk"]);
    }
}
