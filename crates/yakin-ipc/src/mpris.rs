//! MPRIS media players on the session bus (zbus, blocking): the status card lists them and
//! `yakin-cli media pause` (what `desktop:home` runs) pauses the playing ones.

use std::collections::HashMap;

use zbus::blocking::Connection;
use zbus::blocking::Proxy;
use zbus::blocking::fdo::DBusProxy;
use zbus::zvariant::OwnedValue;

/// Bus names of MPRIS players start with this.
const MPRIS: &str = "org.mpris.MediaPlayer2.";

/// One MPRIS player.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Player {
    /// Its bus name.
    pub bus: String,
    /// `Playing`, `Paused` or `Stopped`.
    pub status: String,
    pub title: String,
    pub artists: Vec<String>,
}

/// The bus names of the players that are playing.
pub fn playing(players: &[Player]) -> Vec<&str> {
    players
        .iter()
        .filter(|player| player.status == "Playing")
        .map(|player| player.bus.as_str())
        .collect()
}

/// Pause every player that is playing; how many were asked to. Players that fail are skipped.
pub fn pause_playing() -> usize {
    let Ok(connection) = Connection::session() else {
        return 0;
    };
    let players = players();
    let mut paused = 0;
    for bus in playing(&players) {
        let proxy = Proxy::new(
            &connection,
            bus,
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
        );
        if proxy.is_ok_and(|proxy| proxy.call_method("Pause", &()).is_ok()) {
            paused += 1;
        }
    }
    paused
}

/// Every MPRIS player on the session bus; none when the bus cannot be reached.
pub fn players() -> Vec<Player> {
    let Ok(connection) = Connection::session() else {
        return Vec::new();
    };
    let Ok(dbus) = DBusProxy::new(&connection) else {
        return Vec::new();
    };
    let Ok(names) = dbus.list_names() else {
        return Vec::new();
    };
    names
        .iter()
        .filter(|name| name.starts_with(MPRIS))
        .filter_map(|name| player(&connection, name.as_str()))
        .collect()
}

fn player(
    connection: &Connection,
    name: &str,
) -> Option<Player> {
    let proxy = Proxy::new(
        connection,
        name,
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
    )
    .ok()?;
    let status: String = proxy.get_property("PlaybackStatus").ok()?;
    let metadata: HashMap<String, OwnedValue> = proxy.get_property("Metadata").unwrap_or_default();
    let title = metadata
        .get("xesam:title")
        .and_then(|value| String::try_from(value.clone()).ok())
        .unwrap_or_default();
    let artists = metadata
        .get("xesam:artist")
        .and_then(|value| Vec::<String>::try_from(value.clone()).ok())
        .unwrap_or_default();
    Some(Player {
        bus: name.to_string(),
        status,
        title,
        artists,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_playing_only() {
        let player = |bus: &str, status: &str| Player {
            bus: bus.to_string(),
            status: status.to_string(),
            ..Player::default()
        };
        let players = [
            player("a", "Paused"),
            player("b", "Playing"),
            player("c", "Playing"),
        ];
        assert_eq!(playing(&players), ["b", "c"]);
        assert!(playing(&[]).is_empty());
    }
}
