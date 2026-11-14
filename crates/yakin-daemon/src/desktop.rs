//! The daemon's side of desktop control, shared by every backend: actions out with their
//! failures logged, and the focused window in, reconnecting whenever the desktop is gone.

use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

use crossbeam_channel::Sender;
use log::debug;
use log::info;
use log::warn;
use yakin_core::auto::Auto;
use yakin_core::auto::auto_profile;
use yakin_core::desktop::Action;
use yakin_core::desktop::Desktop;
use yakin_core::desktop::NONE;
use yakin_core::desktop::Window;
use yakin_core::output::Output;

use crate::engine::Event;

/// Failures are logged at most this often, so a missing desktop does not flood the log.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// First wait before watching again; doubles up to [`BACKOFF_MAX`].
const BACKOFF: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Runs the desktop outputs of a report.
pub struct Control {
    desktop: Box<dyn Desktop>,
    warned: Option<Instant>,
}
impl Control {
    pub fn new(desktop: Box<dyn Desktop>) -> Self {
        Self {
            desktop,
            warned: None,
        }
    }

    /// The desktop actions go to, none when there is none.
    pub fn name(&self) -> Option<String> {
        Some(self.desktop.name())
            .filter(|name| *name != NONE)
            .map(String::from)
    }

    /// Run the desktop outputs; failures are logged, never returned.
    pub fn send(
        &mut self,
        outputs: &[Output],
    ) {
        for output in outputs {
            if let Output::Desktop(action) = output {
                self.act(action);
            }
        }
    }

    /// Whether the desktop ran `action`.
    fn act(
        &mut self,
        action: &Action,
    ) -> bool {
        let problem = match self.desktop.act(action) {
            Ok(true) => {
                info!("{action}");
                return true;
            }
            Ok(false) => format!("not supported by desktop {}", self.desktop.name()),
            Err(err) => err.to_string(),
        };
        if self
            .warned
            .is_none_or(|warned| warned.elapsed() >= WARN_EVERY)
        {
            warn!("{action}: {problem}");
            self.warned = Some(Instant::now());
        }
        false
    }
}

/// The selected app, the last focused one `[auto]` does not ignore, with the profile switched
/// to by hand for each window. Focusing an ignored app (yakin's own UI) keeps the selection, so
/// the UI edits and switches for the app it was opened over. Picks are keyed by the window's
/// id, or its app id on a desktop without ids, never by title: the title changes as an app runs
/// (a browser's page, a game's level). For the same reason the rules match a window by the
/// title they first matched it with, so a new title never changes its starting profile. Picks
/// live until the config is reloaded; the selection is focus state and outlives it.
#[derive(Default)]
pub struct Selection {
    app: Option<Window>,
    picks: BTreeMap<Key, String>,
    /// The title a rule first matched each window by.
    titles: BTreeMap<Key, String>,
    /// The profile last active while no rule matched, which leaving a ruled app returns to.
    rest: Option<String>,
}
impl Selection {
    /// Follow a focus change; false for an ignored app, which leaves the selection alone.
    pub fn focus(
        &mut self,
        auto: &Auto,
        focus: Window,
    ) -> bool {
        if auto.ignores(&focus.app_id) {
            return false;
        }
        if let Some(key) = Key::of(&focus)
            && !self.titles.contains_key(&key)
            && auto.rule(&focus.app_id, &focus.title).is_some()
        {
            self.titles.insert(key, focus.title.clone());
        }
        self.app = Some(focus);
        true
    }

    /// The selected app's window, once a non-ignored one was focused.
    pub fn app(&self) -> Option<&Window> {
        self.app.as_ref()
    }

    /// The selected app's title as the rules see it: the one they first matched it by.
    pub fn title(&self) -> &str {
        let Some(app) = self.app.as_ref() else {
            return "";
        };
        Key::of(app)
            .and_then(|key| self.titles.get(&key))
            .unwrap_or(&app.title)
    }

    /// Remember `profile` for the selected app; none or no window, nothing to remember.
    pub fn hold(
        &mut self,
        profile: &str,
    ) {
        if let Some(key) = self.app.as_ref().and_then(Key::of) {
            self.picks.insert(key, profile.to_string());
        }
    }

    /// Whether the selected app has a profile picked by hand.
    pub fn held(&self) -> bool {
        self.app
            .as_ref()
            .and_then(Key::of)
            .is_some_and(|key| self.picks.contains_key(&key))
    }

    /// The profile for the selected app: the one picked by hand, else what `[auto]` picks;
    /// none before a selection, or for one a reload made ignored.
    pub fn pick<'a>(
        &'a self,
        auto: &'a Auto,
    ) -> Option<&'a str> {
        let app = self.app.as_ref()?;
        if auto.ignores(&app.app_id) {
            return None;
        }
        match Key::of(app).and_then(|key| self.picks.get(&key)) {
            Some(profile) => Some(profile),
            None => auto_profile(auto, &app.app_id, self.title()).or(self.rest.as_deref()),
        }
    }

    /// Before focus moves: remember `active` as the rest when the selected app has no rule.
    pub fn settle(
        &mut self,
        auto: &Auto,
        active: &str,
    ) {
        let ruled = self
            .app
            .as_ref()
            .is_some_and(|app| auto.rule(&app.app_id, self.title()).is_some());
        if !ruled {
            self.rest = Some(active.to_string());
        }
    }

    /// Forget the picks, keeping the selection and the matched titles: the config was reloaded.
    pub fn reload(&mut self) {
        self.picks.clear();
    }
}

/// What a pick and a matched title are kept by.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Key {
    Window(u64),
    App(String),
}
impl Key {
    /// None for no window at all.
    fn of(window: &Window) -> Option<Self> {
        match window {
            Window { id: 0, app_id, .. } if app_id.is_empty() => None,
            Window { id: 0, app_id, .. } => Some(Key::App(app_id.clone())),
            Window { id, .. } => Some(Key::Window(*id)),
        }
    }
}

/// Follow the desktop's focus on a thread of its own, watching again whenever it ends.
pub fn start(
    mut desktop: Box<dyn Desktop>,
    tx_event: Sender<Event>,
) {
    let _handle = std::thread::spawn(move || {
        let mut backoff = BACKOFF;
        loop {
            let mut heard = false;
            let mut gone = false;
            let result = desktop.watch(&mut |window| {
                heard = true;
                gone |= tx_event
                    .send(Event::Focus(window.unwrap_or_default()))
                    .is_err();
            });
            if gone {
                return;
            }
            if let Err(err) = result {
                debug!("{} focus: {err}", desktop.name());
            }
            if heard {
                backoff = BACKOFF;
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use yakin_core::Result;
    use yakin_core::profile::Config;

    use super::*;

    /// A desktop that records what it ran and reports a scripted focus.
    struct Fake {
        acts: Arc<Mutex<Vec<Action>>>,
        focus: Vec<Option<Window>>,
    }
    impl Desktop for Fake {
        fn act(
            &mut self,
            action: &Action,
        ) -> Result<bool> {
            self.acts.lock().unwrap().push(action.clone());
            Ok(!matches!(action, Action::Raw(_)))
        }

        fn watch(
            &mut self,
            focus: &mut dyn FnMut(Option<Window>),
        ) -> Result<()> {
            for window in self.focus.drain(..) {
                focus(window);
            }
            Ok(())
        }
    }

    fn window(app_id: &str) -> Option<Window> {
        Some(Window {
            app_id: app_id.to_string(),
            ..Window::default()
        })
    }

    #[test]
    fn runs_actions_and_tells_unsupported() {
        let acts = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            acts: acts.clone(),
            focus: Vec::new(),
        };
        let mut control = Control::new(Box::new(fake));
        assert!(control.act(&Action::Overview));
        assert!(!control.act(&Action::raw("quit").unwrap()));
        control.send(&[Output::Key(1, true), Output::Desktop(Action::Workspace(2))]);
        assert_eq!(
            *acts.lock().unwrap(),
            vec![
                Action::Overview,
                Action::raw("quit").unwrap(),
                Action::Workspace(2)
            ]
        );
    }

    #[test]
    fn focus_drives_auto_rules() {
        let config = Config::parse(
            "[profile.game.buttons]\na = \"key:KEY_A\"\n[profile.desk.buttons]\na = \"key:KEY_B\"\n[daemon]\nactive = \"desk\"\n[auto]\nenabled = true\ndefault = \"desk\"\n[[auto.rule]]\napp_id = \"steam_app_.*\"\nprofile = \"game\"",
        )
        .unwrap();
        let fake = Fake {
            acts: Arc::default(),
            focus: vec![window("steam_app_7"), None, window("foot")],
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        start(Box::new(fake), tx);
        let picked: Vec<Option<&str>> = (0..3)
            .map(|_| match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Event::Focus(focus)) => auto_profile(&config.auto, &focus.app_id, &focus.title),
                _ => panic!("no focus"),
            })
            .collect();
        assert_eq!(picked, vec![Some("game"), Some("desk"), Some("desk")]);
    }

    fn config(ignore: &str) -> Config {
        Config::parse(&format!(
            "[profile.game.buttons]\na = \"key:KEY_A\"\n[profile.desk.buttons]\na = \"key:KEY_B\"\n[daemon]\nactive = \"desk\"\n[auto]\nenabled = true\ndefault = \"desk\"\n{ignore}[[auto.rule]]\napp_id = \"steam_app_.*\"\nprofile = \"game\"",
        ))
        .unwrap()
    }

    #[test]
    fn hand_picks_hold_per_app_until_reload() {
        let config = config("");
        let auto = &config.auto;
        let game = window("steam_app_7").unwrap();
        let foot = window("foot").unwrap();
        let other = window("steam_app_9").unwrap();
        let mut selection = Selection::default();
        // Nothing selected picks nothing and remembers nothing
        assert_eq!(selection.pick(auto), None);
        selection.hold("game");
        assert!(!selection.held());
        // The rules pick on first focus
        assert!(selection.focus(auto, game.clone()));
        assert_eq!(selection.pick(auto), Some("game"));
        // A switch by hand sticks to its app, across focus away and back and a new title
        selection.hold("desk");
        selection.focus(auto, foot.clone());
        assert_eq!(selection.pick(auto), Some("desk"));
        selection.focus(
            auto,
            Window {
                title: "level 2".to_string(),
                ..game.clone()
            },
        );
        assert_eq!(selection.pick(auto), Some("desk"));
        assert!(selection.held());
        // Other apps still follow the rules
        selection.focus(auto, other);
        assert_eq!(selection.pick(auto), Some("game"));
        assert!(!selection.held());
        // No focused window remembers nothing
        selection.focus(auto, Window::default());
        selection.hold("game");
        assert!(!selection.held());
        // A reload starts over
        selection.focus(auto, game);
        selection.reload();
        assert_eq!(selection.pick(auto), Some("game"));
    }

    fn titled(
        id: u64,
        title: &str,
    ) -> Window {
        Window {
            id,
            app_id: String::new(),
            title: title.to_string(),
        }
    }

    #[test]
    fn picks_stick_to_the_window_through_title_changes() {
        let config = Config::parse(
            "[profile.pad]\n[profile.kbm]\n[profile.desk]\n[auto]\nenabled = true\ndefault = \"desk\"\n[[auto.rule]]\napp_id = \"\"\ntitle = \"^friendSLOP$\"\nprofile = \"pad\"",
        )
        .unwrap();
        let auto = &config.auto;
        let mut selection = Selection::default();
        // The rules pick the starting profile, and a new title on the same window keeps it
        selection.focus(auto, titled(5, "friendSLOP"));
        assert_eq!(selection.pick(auto), Some("pad"));
        selection.focus(auto, titled(5, "HALT IS SPEAKING TO PURPLG"));
        assert_eq!(selection.pick(auto), Some("pad"));
        // A switch by hand sticks to the window, though it has no app id
        selection.hold("kbm");
        assert!(selection.held());
        selection.focus(auto, titled(5, "friendSLOP"));
        assert_eq!(selection.pick(auto), Some("kbm"));
        selection.focus(auto, titled(9, "other"));
        assert_eq!(selection.pick(auto), Some("desk"));
        selection.focus(auto, titled(5, "HALT IS SPEAKING TO PURPLG"));
        assert_eq!(selection.pick(auto), Some("kbm"));
        // Another window of the same game starts from the rules
        selection.focus(auto, titled(6, "friendSLOP"));
        assert_eq!(selection.pick(auto), Some("pad"));
        // A reload starts over, from the title the rules first matched
        selection.focus(auto, titled(5, "HALT IS SPEAKING TO PURPLG"));
        selection.reload();
        assert_eq!(selection.pick(auto), Some("pad"));
    }

    #[test]
    fn leaving_a_ruled_app_returns_to_the_rest() {
        let config = Config::parse(
            "[profile.game]\n[profile.desk]\n[profile.couch]\n[auto]\nenabled = true\n[[auto.rule]]\napp_id = \"^nx2navi$\"\nprofile = \"game\"",
        )
        .unwrap();
        let auto = &config.auto;
        let mut selection = Selection::default();
        // Nothing to return to before the first neutral app
        selection.focus(auto, window("nx2navi").unwrap());
        selection.settle(auto, "game");
        selection.focus(auto, window("foot").unwrap());
        assert_eq!(selection.pick(auto), None);
        // The profile last active on a neutral app comes back
        selection.settle(auto, "couch");
        selection.focus(auto, window("nx2navi").unwrap());
        assert_eq!(selection.pick(auto), Some("game"));
        selection.settle(auto, "game");
        selection.focus(auto, window("firefox").unwrap());
        assert_eq!(selection.pick(auto), Some("couch"));
        selection.settle(auto, "couch");
        selection.focus(auto, Window::default());
        assert_eq!(selection.pick(auto), Some("couch"));
    }

    #[test]
    fn focus_after_the_ui_selects_the_next_app() {
        let config = config("");
        let auto = &config.auto;
        let game = window("steam_app_7").unwrap();
        let foot = window("foot").unwrap();
        let ui = window("dev.dsktd.yakin").unwrap();
        let mut selection = Selection::default();
        selection.focus(auto, game.clone());
        assert!(!selection.focus(auto, ui.clone()));
        assert!(selection.focus(auto, foot.clone()));
        assert_eq!(selection.app(), Some(&foot));
        assert_eq!(selection.pick(auto), Some("desk"));
        assert!(!selection.focus(auto, ui));
        assert!(selection.focus(auto, game.clone()));
        assert_eq!(selection.app(), Some(&game));
        assert_eq!(selection.pick(auto), Some("game"));
    }

    #[test]
    fn ignored_apps_keep_the_selected_app() {
        let config = config("ignore = [\"^pavucontrol$\"]\n");
        let auto = &config.auto;
        let ui = window("dev.dsktd.yakin").unwrap();
        let mut selection = Selection::default();
        // Before any app is selected, an ignored one stays unselected and a hand switch is not held
        assert!(!selection.focus(auto, ui.clone()));
        assert_eq!(selection.app(), None);
        selection.hold("game");
        assert!(!selection.held());
        selection.focus(auto, window("foot").unwrap());
        assert_eq!(selection.pick(auto), Some("desk"));
        // Focusing yakin's UI or a configured app keeps foot selected, and its profile
        assert!(!selection.focus(auto, ui.clone()));
        assert!(!selection.focus(auto, window("pavucontrol").unwrap()));
        assert_eq!(selection.app().map(|app| app.app_id.as_str()), Some("foot"));
        assert_eq!(selection.pick(auto), Some("desk"));
        // A switch by hand from the UI is held for foot and applies when it is focused again
        selection.hold("game");
        assert!(selection.held());
        selection.focus(auto, window("steam_app_7").unwrap());
        selection.hold("desk");
        selection.focus(auto, window("foot").unwrap());
        assert_eq!(selection.pick(auto), Some("game"));
        // A reload keeps the selection
        selection.focus(auto, ui);
        selection.reload();
        assert_eq!(selection.app().map(|app| app.app_id.as_str()), Some("foot"));
        assert_eq!(selection.pick(auto), Some("desk"));
    }
}
