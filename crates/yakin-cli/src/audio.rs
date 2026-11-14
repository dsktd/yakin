//! The session's audio outputs over PipeWire: list them, pick one as the default sink, or
//! cycle to the next. Runs in the user's session (the daemon cannot reach its PipeWire).

use std::cell::Cell;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::anyhow;
use pipewire::context::ContextRc;
use pipewire::core::CoreRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::metadata::Metadata;
use pipewire::registry::RegistryRc;
use pipewire::types::ObjectType;
use yakin_core::Result;
use yakin_core::menu::MenuItemToml;
use yakin_core::menu::MenuToml;

/// The metadata key WirePlumber keeps the chosen default sink under.
const CONFIGURED_SINK: &str = "default.configured.audio.sink";

/// The key of the default sink in effect.
const DEFAULT_SINK: &str = "default.audio.sink";

/// Easy Effects' sink: when it runs, it stays the default and its output follows the pick.
const EFFECTS_SINK: &str = "easyeffects_sink";

/// How long Easy Effects gets to move its output to a newly picked default.
const EFFECTS_SETTLE: Duration = Duration::from_secs(1);

/// One sink a stream can play to: the TV's HDMI, an RTP sender to a receiver on the LAN.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
    /// `node.name`, what the default is set to.
    pub name: String,
    /// `node.description`, what the menu shows.
    pub label: String,
}

/// The real outputs and the name of the default sink, if one is set.
#[derive(Clone, Debug, Default)]
pub struct Outputs {
    pub outputs: Vec<Output>,
    pub default: Option<String>,
    /// Easy Effects' sink exists.
    pub effects: bool,
}
impl Outputs {
    /// The output after the default one, wrapping; the first when the default is none of them.
    pub fn next(&self) -> Option<&Output> {
        let at = self
            .outputs
            .iter()
            .position(|output| Some(&output.name) == self.default.as_ref());
        match at {
            Some(at) => self.outputs.get((at + 1) % self.outputs.len()),
            None => self.outputs.first(),
        }
    }

    /// A one-column menu with an item per output, the default one marked, each running
    /// `command <name>` to pick it.
    pub fn menu(
        &self,
        command: &str,
    ) -> MenuToml {
        let items = self
            .outputs
            .iter()
            .map(|output| {
                let mark = match Some(&output.name) == self.default.as_ref() {
                    true => "• ",
                    false => "",
                };
                MenuItemToml {
                    label: format!("{mark}{}", output.label),
                    icon: "volume-up".to_string(),
                    target: format!("desktop:spawn:{command} {}", output.name),
                }
            })
            .collect();
        MenuToml { items, columns: 1 }
    }
}

/// Whether a node with these properties is an output a person would pick: an audio sink that
/// sends sound somewhere, not a null sink an effects chain or a streamer plays into.
fn is_output(props: &dyn Fn(&str) -> Option<String>) -> bool {
    props("media.class").as_deref() == Some("Audio/Sink")
        && props("node.virtual").as_deref() != Some("true")
        && props("factory.name").as_deref() != Some("support.null-audio-sink")
}

/// The `name` of a default-sink metadata value, `{"name":"alsa_output..."}`.
fn sink_name(value: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    value.get("name")?.as_str().map(String::from)
}

/// A connection to the session's PipeWire.
struct Session {
    main_loop: MainLoopRc,
    core: CoreRc,
    registry: RegistryRc,
    // Dropped last
    _context: ContextRc,
}
impl Session {
    fn connect() -> Result<Self> {
        pipewire::init();
        let main_loop =
            MainLoopRc::new(None).map_err(|err| anyhow!("PipeWire main loop: {err}"))?;
        let context =
            ContextRc::new(&main_loop, None).map_err(|err| anyhow!("PipeWire context: {err}"))?;
        let core = context
            .connect_rc(None)
            .map_err(|err| anyhow!("Cannot connect to PipeWire: {err}"))?;
        let registry = core
            .get_registry_rc()
            .map_err(|err| anyhow!("PipeWire registry: {err}"))?;
        Ok(Self {
            main_loop,
            core,
            registry,
            _context: context,
        })
    }

    /// Run the loop until the server has answered everything sent before.
    fn roundtrip(&self) -> Result<()> {
        let done = Rc::new(Cell::new(false));
        let pending = self
            .core
            .sync(0)
            .map_err(|err| anyhow!("PipeWire sync: {err}"))?;
        let done_ = done.clone();
        let main_loop_ = self.main_loop.clone();
        let _listener = self
            .core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pipewire::core::PW_ID_CORE && seq == pending {
                    done_.set(true);
                    main_loop_.quit();
                }
            })
            .register();
        while !done.get() {
            self.main_loop.run();
        }
        Ok(())
    }

    /// Every output and the `default` metadata object, bound.
    fn scan(&self) -> Result<(Outputs, Option<Metadata>)> {
        let outputs = Rc::new(RefCell::new(Vec::new()));
        let effects = Rc::new(Cell::new(false));
        let effects_ = effects.clone();
        let metadata = Rc::new(RefCell::new(None));
        let outputs_ = outputs.clone();
        let metadata_ = metadata.clone();
        let registry_ = self.registry.clone();
        let _listener = self
            .registry
            .add_listener_local()
            .global(move |global| {
                let Some(dict) = global.props else {
                    return;
                };
                let props = |key: &str| dict.get(key).map(String::from);
                match global.type_ {
                    ObjectType::Node if props("node.name").as_deref() == Some(EFFECTS_SINK) => {
                        effects_.set(true);
                    }
                    ObjectType::Node if is_output(&props) => {
                        let Some(name) = props("node.name") else {
                            return;
                        };
                        let label = props("node.description").unwrap_or(name.clone());
                        outputs_.borrow_mut().push(Output { name, label });
                    }
                    ObjectType::Metadata
                        if props("metadata.name").as_deref() == Some("default") =>
                    {
                        if let Ok(bound) = registry_.bind::<Metadata, _>(global) {
                            *metadata_.borrow_mut() = Some(bound);
                        }
                    }
                    _ => {}
                }
            })
            .register();
        self.roundtrip()?;
        let metadata = metadata.borrow_mut().take();
        let default = Rc::new(RefCell::new(None));
        let _listener = metadata.as_ref().map(|metadata| {
            let default_ = default.clone();
            metadata
                .add_listener_local()
                .property(move |_subject, key, _type, value| {
                    if key == Some(DEFAULT_SINK) {
                        *default_.borrow_mut() = value.and_then(sink_name);
                    }
                    0
                })
                .register()
        });
        self.roundtrip()?;
        let mut outputs = outputs.take();
        outputs.sort_by(|a, b| a.label.cmp(&b.label));
        let mut default = default.take();
        // Easy Effects holds the default, so the pick is what we remembered
        if effects.get() && !outputs.iter().any(|o| Some(&o.name) == default.as_ref()) {
            default = std::fs::read_to_string(picked_path())
                .ok()
                .map(|name| name.trim().to_string());
        }
        let outputs = Outputs {
            outputs,
            default,
            effects: effects.get(),
        };
        Ok((outputs, metadata))
    }
}

/// Where the last pick is kept while Easy Effects holds the default sink.
fn picked_path() -> PathBuf {
    match dirs::runtime_dir() {
        Some(dir) => dir.join("yakin-audio-output"),
        None => PathBuf::from(format!("/tmp/yakin-audio-output-{}", nix::unistd::getuid())),
    }
}

/// The outputs and which is the default.
pub fn outputs() -> Result<Outputs> {
    let session = Session::connect()?;
    let (outputs, _metadata) = session.scan()?;
    Ok(outputs)
}

/// Make the output named `name` the default sink; streams that follow the default move to it.
/// Its label, for feedback.
pub fn set(name: &str) -> Result<String> {
    let session = Session::connect()?;
    let (outputs, metadata) = session.scan()?;
    let Some(output) = outputs.outputs.iter().find(|output| output.name == name) else {
        return Err(anyhow!("No audio output {name}"));
    };
    let Some(metadata) = metadata else {
        return Err(anyhow!(
            "PipeWire has no default metadata (is WirePlumber running?)"
        ));
    };
    let configure = |name: &str| {
        let value = serde_json::json!({ "name": name }).to_string();
        metadata.set_property(0, CONFIGURED_SINK, Some("Spa:String:JSON"), Some(&value));
    };
    configure(name);
    session.roundtrip()?;
    if outputs.effects {
        // Easy Effects' output follows the default: let it move, then take the default back so
        // the EQ stays in the path
        std::thread::sleep(EFFECTS_SETTLE);
        configure(EFFECTS_SINK);
        session.roundtrip()?;
        std::fs::write(picked_path(), name)?;
    }
    Ok(output.label.clone())
}

#[cfg(test)]
mod test {
    use super::*;

    fn outputs(default: Option<&str>) -> Outputs {
        Outputs {
            outputs: ["hdmi", "rtp-desktop", "rtp-kitchen"]
                .iter()
                .map(|name| Output {
                    name: name.to_string(),
                    label: name.to_uppercase(),
                })
                .collect(),
            default: default.map(String::from),
            effects: false,
        }
    }

    #[test]
    fn test_next_wraps() {
        let next = |default| outputs(default).next().map(|output| output.name.clone());
        assert_eq!(next(Some("hdmi")).as_deref(), Some("rtp-desktop"));
        assert_eq!(next(Some("rtp-kitchen")).as_deref(), Some("hdmi"));
        // Easy Effects' sink or none: start at the first
        assert_eq!(next(Some("easyeffects_sink")).as_deref(), Some("hdmi"));
        assert_eq!(next(None).as_deref(), Some("hdmi"));
        assert!(Outputs::default().next().is_none());
    }

    #[test]
    fn test_menu_marks_default_and_parses() {
        let toml = outputs(Some("rtp-desktop")).menu("/bin/yakin-cli audio set");
        assert_eq!(toml.columns, 1);
        assert_eq!(toml.items[1].label, "• RTP-DESKTOP");
        assert_eq!(toml.items[0].label, "HDMI");
        assert_eq!(
            toml.items[2].target,
            "desktop:spawn:/bin/yakin-cli audio set rtp-kitchen"
        );
        assert!(yakin_core::menu::Menu::try_from(&toml).is_ok());
    }

    #[test]
    fn test_only_real_sinks() {
        let node = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(known, _)| *known == key)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert!(is_output(&node(&[("media.class", "Audio/Sink")])));
        assert!(!is_output(&node(&[("media.class", "Audio/Source")])));
        assert!(!is_output(&node(&[
            ("media.class", "Audio/Sink"),
            ("factory.name", "support.null-audio-sink"),
        ])));
        assert!(!is_output(&node(&[
            ("media.class", "Audio/Sink"),
            ("node.virtual", "true"),
        ])));
    }

    #[test]
    fn test_sink_name() {
        assert_eq!(sink_name(r#"{"name":"hdmi"}"#).as_deref(), Some("hdmi"));
        assert_eq!(sink_name("nope"), None);
    }
}
