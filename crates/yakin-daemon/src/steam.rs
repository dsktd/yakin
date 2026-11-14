//! Whether the daemon has stepped aside for Steam holding the controller: which focused windows
//! Steam Input drives, whether it is on for a game, and the yield itself.
//!
//! Steam Input being off for a game is read from Steam's own setting (`UseSteamControllerConfig`
//! under the app in `userdata/<id>/config/localconfig.vdf`, "0" = forced off): one documented
//! key, written by Steam itself. Looking for Steam's virtual gamepad under `/dev/input` instead
//! would be no better: Steam creates it for Big Picture and the desktop layout too, and keeps it
//! around after a game ends, so its presence says little about the focused game.

use std::path::Path;
use std::path::PathBuf;

use log::debug;
use yakin_core::desktop::Window;
use yakin_core::device::holders::Holder;

/// Where Steam keeps its per-user config, relative to home: native, the `~/.steam` link, flatpak.
const STEAM_DIRS: [&str; 3] = [
    ".local/share/Steam",
    ".steam/steam",
    ".var/app/com.valvesoftware.Steam/.local/share/Steam",
];

/// The per-app key; "0" is forced off, "1" the global default, "2" forced on.
const USE_STEAM_INPUT: &str = "UseSteamControllerConfig";

/// A focused window Steam Input drives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Game {
    /// A Steam game by its app id, from the `steam_app_<appid>` window.
    App(String),
    BigPicture,
    /// A gamescope session, whatever runs in it.
    Gamescope,
}
impl Game {
    pub fn of(window: &Window) -> Option<Self> {
        if let Some(appid) = window.app_id.strip_prefix("steam_app_") {
            return Some(Game::App(appid.to_string()));
        }
        match window.app_id.as_str() {
            "steam" if window.title.to_lowercase().contains("big picture") => {
                Some(Game::BigPicture)
            }
            "gamescope" => Some(Game::Gamescope),
            _ => None,
        }
    }

    /// Whether Steam Input is turned off for this game in Steam's settings; false when it cannot
    /// be told, so the daemon yields as for any game.
    pub fn steam_input_disabled(&self) -> bool {
        let Game::App(appid) = self else {
            return false;
        };
        let Some(path) = dirs::home_dir().and_then(|home| localconfig(&home)) else {
            debug!("No localconfig.vdf found");
            return false;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            debug!("Cannot read {}", path.display());
            return false;
        };
        disabled_in(&text, appid)
    }
}
impl std::fmt::Display for Game {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Game::App(appid) => write!(f, "steam_app_{appid}"),
            Game::BigPicture => write!(f, "Big Picture"),
            Game::Gamescope => write!(f, "gamescope"),
        }
    }
}

/// The most recently written `localconfig.vdf` of any Steam user under `home`.
fn localconfig(home: &Path) -> Option<PathBuf> {
    STEAM_DIRS
        .iter()
        .filter_map(|dir| std::fs::read_dir(home.join(dir).join("userdata")).ok())
        .flatten()
        .filter_map(|user| Some(user.ok()?.path().join("config/localconfig.vdf")))
        .filter_map(|path| Some((std::fs::metadata(&path).ok()?.modified().ok()?, path)))
        .max()
        .map(|(_modified, path)| path)
}

/// Whether `text`, a localconfig.vdf, turns Steam Input off for `appid`. Steam has kept the
/// apps under `Software/Valve/Steam/apps` and directly under the root over the years.
fn disabled_in(
    text: &str,
    appid: &str,
) -> bool {
    let Ok(vdf) = keyvalues_parser::parse(text.trim_start_matches('\u{feff}')) else {
        debug!("localconfig.vdf does not parse");
        return false;
    };
    let Some(root) = vdf.value.get_obj() else {
        return false;
    };
    let apps = [&["Software", "Valve", "Steam", "apps"][..], &["apps"][..]];
    apps.iter()
        .filter_map(|keys| keys.iter().try_fold(root, |obj, key| obj_of(obj, key)))
        .filter_map(|apps| obj_of(apps, appid))
        .filter_map(|app| text_of(app, USE_STEAM_INPUT))
        .any(|value| value == "0")
}

fn obj_of<'a>(
    obj: &'a keyvalues_parser::Obj<'a>,
    key: &str,
) -> Option<&'a keyvalues_parser::Obj<'a>> {
    obj.iter()
        .filter(|(known, _)| known.eq_ignore_ascii_case(key))
        .flat_map(|(_, values)| values.iter())
        .find_map(keyvalues_parser::Value::get_obj)
}

fn text_of<'a>(
    obj: &'a keyvalues_parser::Obj<'a>,
    key: &str,
) -> Option<&'a str> {
    obj.iter()
        .filter(|(known, _)| known.eq_ignore_ascii_case(key))
        .flat_map(|(_, values)| values.iter())
        .find_map(keyvalues_parser::Value::get_str)
}

/// What the engine should do after looking at the holders.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Step {
    Stay,
    Yield(Holder),
    Reclaim,
}

/// The process the daemon yielded to, if any.
#[derive(Default)]
pub struct Yield(Option<Holder>);
impl Yield {
    pub fn holder(&self) -> Option<&Holder> {
        self.0.as_ref()
    }

    /// Yield to the first foreign holder while `wanted`; reclaim once there is none or it is
    /// not wanted any more.
    pub fn step(
        &self,
        holders: &[Holder],
        wanted: bool,
    ) -> Step {
        match (&self.0, holders.first(), wanted) {
            (None, Some(holder), true) => Step::Yield(holder.clone()),
            (Some(_), None, _) | (Some(_), _, false) => Step::Reclaim,
            _ => Step::Stay,
        }
    }

    pub fn yielded(
        &mut self,
        holder: Holder,
    ) {
        self.0 = Some(holder);
    }

    pub fn reclaimed(&mut self) {
        self.0 = None;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn steam(pid: u32) -> Holder {
        Holder {
            pid,
            comm: "steam".to_string(),
        }
    }

    #[test]
    fn test_yield() {
        let mut state = Yield::default();
        assert_eq!(state.step(&[], true), Step::Stay);
        assert_eq!(
            state.step(&[steam(1), steam(2)], true),
            Step::Yield(steam(1))
        );
        // Held, but no reason to step aside
        assert_eq!(state.step(&[steam(1)], false), Step::Stay);
        state.yielded(steam(1));
        assert_eq!(state.holder(), Some(&steam(1)));
        // Still held, by the same process or another
        assert_eq!(state.step(&[steam(1)], true), Step::Stay);
        assert_eq!(state.step(&[steam(2)], true), Step::Stay);
        assert_eq!(state.step(&[], true), Step::Reclaim);
        // Still held, but the game lost focus
        assert_eq!(state.step(&[steam(1)], false), Step::Reclaim);
        // A failed reclaim leaves it yielded, so the next tick tries again
        assert_eq!(state.step(&[], true), Step::Reclaim);
        state.reclaimed();
        assert_eq!(state.holder(), None);
        assert_eq!(state.step(&[], true), Step::Stay);
    }

    fn window(
        app_id: &str,
        title: &str,
    ) -> Window {
        Window {
            app_id: app_id.to_string(),
            title: title.to_string(),
            ..Window::default()
        }
    }

    #[test]
    fn test_game() {
        assert_eq!(
            Game::of(&window("steam_app_1234", "Game")),
            Some(Game::App("1234".to_string()))
        );
        assert_eq!(
            Game::of(&window("steam", "Steam Big Picture Mode")),
            Some(Game::BigPicture)
        );
        assert_eq!(Game::of(&window("steam", "Steam")), None);
        assert_eq!(Game::of(&window("steam", "Friends List")), None);
        assert_eq!(Game::of(&window("gamescope", "")), Some(Game::Gamescope));
        assert_eq!(Game::of(&window("foot", "steam_app_1")), None);
        assert_eq!(Game::of(&Window::default()), None);
        assert_eq!(Game::App("7".to_string()).to_string(), "steam_app_7");
        assert!(!Game::BigPicture.steam_input_disabled());
    }

    const LOCALCONFIG: &str = r#""UserLocalConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"apps"
				{
					"1234"
					{
						"LastPlayed"		"1700000000"
						"UseSteamControllerConfig"		"0"
					}
					"5678"
					{
						"UseSteamControllerConfig"		"2"
					}
					"9"
					{
						"LastPlayed"		"1700000000"
					}
				}
			}
		}
	}
	"apps"
	{
		"42"
		{
			"usesteamcontrollerconfig"		"0"
		}
	}
}
"#;

    #[test]
    fn test_disabled_in() {
        assert!(disabled_in(LOCALCONFIG, "1234"));
        assert!(!disabled_in(LOCALCONFIG, "5678"));
        assert!(!disabled_in(LOCALCONFIG, "9"));
        assert!(!disabled_in(LOCALCONFIG, "1"));
        assert!(disabled_in(LOCALCONFIG, "42"));
        assert!(!disabled_in("not vdf {", "1234"));
        assert!(!disabled_in("\"x\" \"y\"", "1234"));
    }

    #[test]
    fn test_localconfig() {
        let home = std::env::temp_dir().join(format!("yakin-steam-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(localconfig(&home), None);
        let old = home.join(".local/share/Steam/userdata/1/config");
        let new = home.join(".steam/steam/userdata/2/config");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("localconfig.vdf"), "").unwrap();
        std::fs::write(new.join("localconfig.vdf"), "").unwrap();
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::open(old.join("localconfig.vdf"))
            .unwrap()
            .set_modified(then)
            .unwrap();
        let found = localconfig(&home);
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(found, Some(new.join("localconfig.vdf")));
    }
}
