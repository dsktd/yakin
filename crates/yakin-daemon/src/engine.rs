//! The daemon's one event loop: controller input in, outputs and IPC answers out.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use crossbeam_channel::Receiver;
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::Sender;
use log::debug;
use log::info;
use log::warn;
use nix::time::ClockId;
use nix::time::clock_gettime;
use yakin_core::Result;
use yakin_core::auto::auto_profile;
use yakin_core::desktop::Window;
use yakin_core::device::ControllerId;
use yakin_core::device::Event as DeviceEvent;
use yakin_core::device::bluetooth;
use yakin_core::device::grab::Grabs;
use yakin_core::device::haptics;
use yakin_core::device::haptics::Haptics;
use yakin_core::device::haptics::Limiter;
use yakin_core::device::hidraw;
use yakin_core::device::hidraw::Hidraw;
use yakin_core::device::hidraw::Pause;
use yakin_core::device::holders;
use yakin_core::device::holders::Holder;
use yakin_core::device::ibex;
use yakin_core::device::ibex::Wireless;
use yakin_core::keyboard::Keyboard;
use yakin_core::lizard::Lizard;
use yakin_core::menu::Menu;
use yakin_core::menu::MenuCommand;
use yakin_core::menu::MenuInput;
use yakin_core::menu::MenuNav;
use yakin_core::output::Keys;
use yakin_core::output::Mouse;
use yakin_core::output::Output;
use yakin_core::output::Pad;
use yakin_core::output::motion::MotionSocket;
use yakin_core::output::motion::Sample;
use yakin_core::output::motion::monotonic_micros;
use yakin_core::power::Command;
use yakin_core::power::Cue;
use yakin_core::power::Fact;
use yakin_core::power::Outcome;
use yakin_core::power::SleepPhase;
use yakin_core::power::Source;
use yakin_core::power::Tv;
use yakin_core::power::TvPower;
use yakin_core::profile::Config;
use yakin_core::profile::Mapper;
use yakin_core::profile::PAD;
use yakin_core::profile::Profile;
use yakin_core::profile::ProfileSwitch;
use yakin_core::profile::RELEASE_GUARD_MAX;
use yakin_core::profile::TICK_MS;
use yakin_core::profile::Target;
use yakin_core::profile::Vocabulary;
use yakin_core::profile::Yielding;
use yakin_core::profile::tap_outputs;
use yakin_core::sleep::Sleeps;
use yakin_core::state::Button;
use yakin_core::state::ControllerState;
use yakin_core::state::Side;
use yakin_ipc::IpcAuto;
use yakin_ipc::IpcConfig;
use yakin_ipc::IpcController;
use yakin_ipc::IpcEvent;
use yakin_ipc::IpcHolder;
use yakin_ipc::IpcKeyboard;
use yakin_ipc::IpcLink;
use yakin_ipc::IpcMenu;
use yakin_ipc::IpcRequest;
use yakin_ipc::IpcResponse;
use yakin_ipc::IpcStatus;
use yakin_ipc::IpcTuning;
use yakin_ipc::server::Subscriber;

use crate::config::Layers;
use crate::desktop::Control;
use crate::desktop::Selection;
use crate::steam::Game;
use crate::steam::Step;
use crate::steam::Yield;

/// How long a suspending machine is held up for the controllers and the TV to be told.
const SLEEP_WAIT: Duration = Duration::from_secs(2);

/// How long tuning of a profile must rest before it is written, so a dragged slider writes once.
const SETTLE: Duration = Duration::from_secs(1);

pub enum Event {
    Device(DeviceEvent),
    Ipc(IpcRequest, Sender<IpcResponse>),
    Subscribe(Subscriber),
    /// Time to look at who else holds the controller.
    Tick,
    /// The desktop focused another window, or the focused one changed its title; empty for
    /// none.
    Focus(Window),
    /// Something observed about the TV, for the power owner.
    Tv(Fact),
    /// The daemon is asked to stop: write what is pending, then end the loop.
    Shutdown,
}
impl From<DeviceEvent> for Event {
    fn from(event: DeviceEvent) -> Self {
        Event::Device(event)
    }
}
impl From<(IpcRequest, Sender<IpcResponse>)> for Event {
    fn from((request, tx_response): (IpcRequest, Sender<IpcResponse>)) -> Self {
        Event::Ipc(request, tx_response)
    }
}

impl From<Subscriber> for Event {
    fn from(subscriber: Subscriber) -> Self {
        Event::Subscribe(subscriber)
    }
}

/// An open hidraw node, whether or not it has sent input yet.
struct Device {
    path: PathBuf,
    /// The link it sits on (`HID_PHYS`), shared with the controller's own keyboard and mouse
    /// there.
    phys: Option<String>,
    battery: Option<u8>,
    wireless: bool,
    link: IpcLink,
    /// A controller is on it; see [`IpcController::connected`].
    connected: bool,
    /// Whether the firmware's keyboard and mouse are on, as last commanded; None until the
    /// reader has set the node up.
    lizard: Option<bool>,
    /// Handed to its own keyboard and mouse by a `lizard:` binding.
    handed: bool,
    /// Bursts of its own keyboard and mouse seen while lizard mode should have been off.
    leaks: u32,
}

/// A controller that has sent input, with its virtual pad.
struct Controller {
    pad: Pad,
    mapper: Mapper,
    prev: ControllerState,
    /// Reads the controller while the menu is open.
    nav: MenuNav,
    /// Its hidraw node for haptic pulses, unless that failed to open.
    haptics: Option<Haptics>,
    limiter: Limiter,
    /// In lizard mode by a `lizard:` binding: nothing goes out until it ends.
    lizard: Lizard,
}

pub struct Engine {
    layers: Layers,
    config: Config,
    active: String,
    profile: Profile,
    lizard_off: bool,
    /// Dropped while yielded, like the pads.
    keys: Option<Keys>,
    mouse: Option<Mouse>,
    /// Raw motion of the first controller while its profile maps, by `daemon.motion_socket`
    /// at start.
    motion: Option<MotionSocket>,
    desktop: Control,
    pause: Pause,
    yielded: Yield,
    /// The focused window, ignored apps included.
    focus: Window,
    /// What Steam Input drives in the focused window, when it is on for it: a reason to yield.
    game: Option<Game>,
    /// The controller's own keyboard and mouse, held while yakin drives it.
    grabs: Grabs,
    devices: BTreeMap<ControllerId, Device>,
    controllers: BTreeMap<ControllerId, Controller>,
    subscribers: Vec<Subscriber>,
    /// The open quick menu: every controller drives it instead of the profile.
    menu: Option<Menu>,
    /// The open on-screen keyboard: every controller types on it instead of the profile.
    keyboard: Option<Keyboard>,
    /// The selected app and the profiles switched to by hand per app, which win over
    /// `[auto]` until the config reloads.
    selection: Selection,
    /// Profiles tuned since their knobs were last written, with when they were last tuned.
    unsaved: BTreeMap<String, Instant>,
    /// A failed save was logged; quiet until one succeeds.
    save_failed: bool,
    /// What yakin believes of the TV's power; in standby nothing goes out until a button
    /// wakes it.
    tv: Tv,
    /// Looks for a sleep nobody announced.
    sleeps: Sleeps,
    /// While the machine suspends: held by every command's thread until it is done, so the
    /// hook's request is answered after.
    wait: Option<Sender<()>>,
    /// Where background work reports back.
    tx_event: Sender<Event>,
}
impl Engine {
    pub fn new(
        layers: Layers,
        config: Config,
        lizard_off: bool,
        pause: Pause,
        tx_event: Sender<Event>,
    ) -> Result<Self> {
        let active = config.active()?;
        let profile = config.get(&active)?;
        info!("Profile {active}");
        let (tv, commands) = Tv::new(Instant::now(), config.daemon.tv_settings());
        let motion = match config.daemon.motion_socket {
            true => {
                let path = yakin_ipc::path::motion_socket();
                MotionSocket::bind(&path)
                    .inspect(|_| info!("Motion socket {}", path.display()))
                    .inspect_err(|err| warn!("Motion socket {}: {err}", path.display()))
                    .ok()
            }
            false => None,
        };
        let mut engine = Self {
            layers,
            keys: Some(Keys::new(config.keys())?),
            mouse: Some(Mouse::new()?),
            motion,
            desktop: Control::new(config.desktop.desktop()),
            config,
            active,
            profile,
            lizard_off,
            pause,
            yielded: Yield::default(),
            focus: Window::default(),
            game: None,
            grabs: Grabs::default(),
            devices: BTreeMap::new(),
            controllers: BTreeMap::new(),
            subscribers: Vec::new(),
            menu: None,
            keyboard: None,
            selection: Selection::default(),
            unsaved: BTreeMap::new(),
            save_failed: false,
            tv,
            sleeps: Sleeps::default(),
            wait: None,
            tx_event,
        };
        engine.run(None, commands);
        Ok(engine)
    }

    pub fn handle(
        &mut self,
        event: Event,
    ) {
        match event {
            Event::Device(event) => self.device(event),
            Event::Ipc(IpcRequest::Sleep(phase), tx_response) => {
                let done = self.sleep(phase);
                let _handle = std::thread::spawn(move || {
                    // Every command's thread drops its end when done
                    if let Err(RecvTimeoutError::Timeout) = done.recv_timeout(SLEEP_WAIT) {
                        warn!("Sleep: commands still running");
                    }
                    let _ = tx_response.send(IpcResponse::Ok);
                });
            }
            Event::Ipc(request, tx_response) => {
                let response = match self.request(request) {
                    Ok(response) => response,
                    Err(err) => IpcResponse::Error(err.to_string()),
                };
                let _ = tx_response.send(response);
            }
            Event::Subscribe(subscriber) => {
                subscriber.event(IpcEvent::Profile(self.active.clone()));
                subscriber.event(IpcEvent::Controllers(self.status()));
                subscriber.event(IpcEvent::Yielded(self.holder()));
                subscriber.event(IpcEvent::Menu(self.menu.as_ref().map(IpcMenu::from)));
                subscriber.event(IpcEvent::Keyboard(
                    self.keyboard.as_ref().map(IpcKeyboard::from),
                ));
                self.subscribers.push(subscriber);
            }
            Event::Tick => {
                // Gone clients close their slot themselves; drop them even when nothing is sent
                self.subscribers
                    .retain(|subscriber| !subscriber.is_closed());
                self.check();
                self.hold();
                self.watch();
                if self.sleeps.slept(
                    clock(ClockId::CLOCK_BOOTTIME),
                    clock(ClockId::CLOCK_MONOTONIC),
                ) {
                    info!("Slept");
                    self.sleep(SleepPhase::Resumed);
                }
            }
            Event::Tv(fact) => {
                let commands = self.tv.fact(&fact, Instant::now());
                self.run(None, commands);
            }
            Event::Shutdown => {
                self.flush();
                // Release what is held and drop the pads, which stop their motors on the way
                let ids: Vec<ControllerId> = self.controllers.keys().copied().collect();
                for id in ids {
                    self.disconnect(id);
                }
            }
            Event::Focus(focus) => {
                debug!("Focus {} {:?}", focus.app_id, focus.title);
                self.focus = focus.clone();
                self.spot();
                self.check();
                self.selection.settle(&self.config.auto, &self.active);
                if self.selection.focus(&self.config.auto, focus) {
                    self.follow();
                    self.publish(IpcEvent::Auto(self.auto()));
                }
            }
        }
    }

    fn device(
        &mut self,
        event: DeviceEvent,
    ) {
        match event {
            DeviceEvent::Opened(id, path) => {
                debug!("{id} opened {}", path.display());
                // A pad left from an earlier node of the same name starts over
                self.disconnect(id);
                let phys = hidraw::phys(&path);
                let link = match hidraw::product(&path) {
                    Some(ibex::PRODUCT_WIRED) => IpcLink::Wired,
                    Some(ibex::PRODUCT_BLE) => IpcLink::Bluetooth,
                    _ => IpcLink::Puck,
                };
                self.devices.insert(
                    id,
                    Device {
                        path,
                        phys,
                        battery: None,
                        wireless: false,
                        link,
                        connected: link != IpcLink::Puck,
                        lizard: None,
                        handed: false,
                        leaks: 0,
                    },
                );
                self.publish(IpcEvent::Controllers(self.status()));
                self.check();
                self.hold();
            }
            DeviceEvent::State(id, state) => {
                // Reports already on their way when the readers paused
                if self.yielded.holder().is_some() {
                    return;
                }
                // Reports from a node already closed; the status lists only open ones
                if !self.devices.contains_key(&id) {
                    debug!("{id} report from a closed node");
                    return;
                }
                if let Some(device) = self.devices.get_mut(&id)
                    && !device.connected
                {
                    device.connected = true;
                    self.publish(IpcEvent::Controllers(self.status()));
                }
                if !self.controllers.contains_key(&id) {
                    let path = self.devices.get(&id).map(|device| device.path.clone());
                    let haptics = || {
                        let path = path.as_ref()?;
                        Haptics::open(path)
                            .inspect_err(|err| warn!("{id} haptics: {err}"))
                            .ok()
                    };
                    let phys = path.as_ref().map(|path| path.display().to_string());
                    let rumble = self.config.daemon.rumble.then(haptics).flatten();
                    let pad = match Pad::new(&phys.unwrap_or(id.to_string()), rumble) {
                        Ok(pad) => pad,
                        Err(err) => {
                            warn!("{id} pad: {err}");
                            return;
                        }
                    };
                    let haptics = haptics();
                    info!("{id} connected");
                    // A connect is no press: it never wakes the TV, as a Bluetooth controller
                    // reconnects by itself after tv:off
                    let commands = self.tv.controller_connected();
                    self.run(Some(id), commands);
                    // Left on by a binding when the link last went away: the reader only
                    // turns it off again on the link's own connect report, which not every
                    // return brings
                    if self.lizard_off && self.devices[&id].lizard == Some(true) {
                        self.set_lizard(id, false);
                    }
                    let controller = Controller {
                        pad,
                        mapper: Mapper::new(self.profile.clone()),
                        prev: ControllerState::default(),
                        nav: MenuNav::default(),
                        haptics,
                        limiter: Limiter::default(),
                        lizard: Lizard::default(),
                    };
                    self.controllers.insert(id, controller);
                }
                self.subscribers
                    .retain(|subscriber| !subscriber.is_closed());
                for subscriber in &self.subscribers {
                    subscriber.state(u8::from(id), state);
                }
                let close = self.menu_buttons();
                let close_keyboard = self.keyboard_buttons();
                let first = self.controllers.keys().next() == Some(&id);
                let controller = self.controllers.get_mut(&id).unwrap();
                // The menu and keyboard read the sticks as the profile does (stick.touch)
                let seen = controller.mapper.touched(&state, Instant::now());
                if self.tv.gating() {
                    let (_, commands) = self.tv.filter(
                        &controller.prev,
                        &state,
                        Vec::new(),
                        Instant::now(),
                        self.config.daemon.tv_wake(),
                    );
                    controller.prev = state;
                    self.run(Some(id), commands);
                    return;
                }
                if let Some(keyboard) = &mut self.keyboard {
                    let before = keyboard.clone();
                    let outputs = keyboard.read(
                        &controller.prev,
                        &seen,
                        &close_keyboard,
                        self.profile.haptics_menu,
                    );
                    controller.prev = state;
                    if before != *keyboard {
                        self.publish_keyboard();
                    }
                    self.perform(id, &outputs);
                    return;
                }
                if self.menu.is_some() {
                    let inputs =
                        controller
                            .nav
                            .read(&controller.prev, &seen, &close, Instant::now());
                    controller.prev = state;
                    self.navigate(id, inputs);
                    return;
                }
                // Only where the profile maps, as the gyro binding would
                if let Some(motion) = &mut self.motion
                    && first
                    && !controller.lizard.is_on()
                {
                    let held = self.profile.gyro_engaged(state.buttons);
                    motion.send(Sample::new(monotonic_micros(), state.imu).held(held));
                }
                let outputs = controller.mapper.map(&controller.prev, &state);
                let (outputs, commands) = self.tv.filter(
                    &controller.prev,
                    &state,
                    outputs,
                    Instant::now(),
                    self.config.daemon.tv_wake(),
                );
                controller.prev = state;
                if !commands.is_empty() {
                    // A standby's report may carry releases, which must go out before it
                    self.perform(id, &outputs);
                    self.run(Some(id), commands);
                    return;
                }
                let (outputs, change) = controller.lizard.filter(outputs);
                match change {
                    Some(true) => self.lizard(id, true),
                    Some(false) => self.lizard(id, false),
                    None => self.perform(id, &outputs),
                }
            }
            DeviceEvent::Battery(id, battery) => {
                debug!("{id} battery {battery:?}");
                if let Some(device) = self.devices.get_mut(&id) {
                    device.battery = Some(battery.level);
                }
                self.publish(IpcEvent::Controllers(self.status()));
            }
            DeviceEvent::Wireless(id, wireless) => {
                debug!("{id} {wireless:?}");
                if let Some(device) = self.devices.get_mut(&id) {
                    device.wireless = true;
                    match wireless {
                        Wireless::Connect => device.connected = true,
                        Wireless::Disconnect => device.connected = false,
                        Wireless::Pair | Wireless::Unknown(_) => {}
                    }
                }
                if wireless == Wireless::Disconnect {
                    // The level of a controller that left the slot would be shown for the
                    // next one
                    if let Some(device) = self.devices.get_mut(&id) {
                        device.battery = None;
                    }
                    self.disconnect(id);
                }
                self.publish(IpcEvent::Controllers(self.status()));
            }
            DeviceEvent::Lizard(id, on) => {
                if let Some(device) = self.devices.get_mut(&id) {
                    device.lizard = Some(on);
                }
                self.publish(IpcEvent::Controllers(self.status()));
                self.hold();
            }
            DeviceEvent::Closed(id) => {
                debug!("{id} closed");
                self.devices.remove(&id);
                self.disconnect(id);
                self.publish(IpcEvent::Controllers(self.status()));
                self.hold();
            }
        }
    }

    fn request(
        &mut self,
        request: IpcRequest,
    ) -> Result<IpcResponse> {
        match request {
            IpcRequest::Status => {
                let profiles = self.profiles();
                let tunings = profiles
                    .iter()
                    .map(|name| Ok(tuning(name, &self.config.get(name)?)))
                    .collect::<Result<_>>()?;
                Ok(IpcResponse::Status(IpcStatus {
                    profile: self.active.clone(),
                    controllers: self.status(),
                    profiles,
                    tunings,
                    yielded: self.holder(),
                    auto: self.auto(),
                    desktop: self.desktop.name(),
                }))
            }
            IpcRequest::Activate(name) => {
                let profile = self.config.get(&name)?;
                self.hold_pick(&name);
                self.activate(name, profile);
                Ok(IpcResponse::Ok)
            }
            IpcRequest::Reload => {
                let (_, config, skipped) = self.layers.load_lenient()?;
                for warning in skipped {
                    warn!("{warning}");
                }
                self.apply(config)?;
                Ok(IpcResponse::Ok)
            }
            IpcRequest::Tune(tuning) => {
                self.tune(&tuning)?;
                self.unsaved.insert(tuning.profile.clone(), Instant::now());
                self.publish(IpcEvent::Tuning(tuning));
                Ok(IpcResponse::Ok)
            }
            IpcRequest::GetConfig => Ok(IpcResponse::Config(Box::new(self.config_file()?))),
            IpcRequest::SetConfig(toml) => {
                let config = self.layers.save(&toml)?;
                self.apply(config)?;
                self.publish(IpcEvent::Config(Box::new(self.config_file()?)));
                Ok(IpcResponse::Ok)
            }
            IpcRequest::Vocabulary => Ok(IpcResponse::Vocabulary(Vocabulary::new())),
            IpcRequest::Reclaim => {
                if self.yielded.holder().is_some() {
                    self.reclaim()?;
                    info!("Reclaimed the controller: asked to");
                }
                Ok(IpcResponse::Ok)
            }
            IpcRequest::ControllerOff => {
                self.run(None, vec![Command::ControllersOff]);
                let fact = Fact::TvPower(TvPower::Standby, Source::Hook);
                let commands = self.tv.fact(&fact, Instant::now());
                self.run(None, commands);
                Ok(IpcResponse::Ok)
            }
            IpcRequest::Menu(command) => {
                // Yielded there is no controller to drive it, and reclaiming would land in it
                if command.change(self.menu.is_some()) == Some(true) {
                    self.ensure_held()?;
                }
                self.command_menu(command);
                Ok(IpcResponse::Ok)
            }
            IpcRequest::ShowMenu(toml) => {
                let menu = Menu::try_from(&toml)?;
                self.ensure_held()?;
                self.open_menu(menu);
                Ok(IpcResponse::Ok)
            }
            IpcRequest::Keyboard(command) => {
                if command.change(self.keyboard.is_some()) == Some(true) {
                    self.ensure_held()?;
                }
                self.command_keyboard(command);
                Ok(IpcResponse::Ok)
            }
            // The event loop answers it once the commands are done
            IpcRequest::Sleep(_) => Err(anyhow!("Sleep is not a plain request")),
            // The server answers it itself and sends a Subscriber instead
            IpcRequest::Subscribe => Err(anyhow!("Subscribe is not a request")),
        }
    }

    /// Send `outputs` of controller `id`, then switch profile or menu as the last of them asks.
    fn perform(
        &mut self,
        id: ControllerId,
        outputs: &[Output],
    ) {
        if let Some(controller) = self.controllers.get_mut(&id) {
            send(
                id,
                controller,
                self.keys.as_mut(),
                self.mouse.as_mut(),
                outputs,
            );
        }
        self.desktop.send(outputs);
        // Last in the report wins; the rest went out under the old profile above
        let switch = outputs.iter().rev().find_map(|output| match output {
            Output::Profile(switch) => Some(switch),
            _ => None,
        });
        if let Some(switch) = switch {
            self.switch(&switch.clone());
        }
        let command = outputs.iter().rev().find_map(|output| match output {
            Output::Menu(command) => Some(*command),
            _ => None,
        });
        if let Some(command) = command {
            self.command_menu(command);
        }
        let command = outputs.iter().rev().find_map(|output| match output {
            Output::Keyboard(command) => Some(*command),
            _ => None,
        });
        if let Some(command) = command {
            self.command_keyboard(command);
        }
    }

    /// Run what the TV's owner asks. Whatever controller `id` holds is let go when it wakes the
    /// TV, none of it went out; on standby everything held goes up before the controllers go.
    fn run(
        &mut self,
        id: Option<ControllerId>,
        commands: Vec<Command>,
    ) {
        for command in commands {
            match command {
                Command::TvQuery => self.query_tv(),
                Command::TvWake => {
                    let controller = id.and_then(|id| self.controllers.get_mut(&id));
                    if let Some(controller) = controller {
                        controller.mapper.release_all();
                    }
                    info!("TV wake");
                    self.shell(command, self.config.daemon.tv_on.clone());
                }
                Command::TvActiveSource => {
                    info!("TV active source");
                    self.shell(command, self.config.daemon.tv_on.clone());
                }
                Command::TvStandby => {
                    info!("TV standby");
                    self.shell(command, self.config.daemon.tv_off.clone());
                }
                Command::Say(cue) => self.say(cue),
                Command::ControllersOff => {
                    for (id, controller) in &mut self.controllers {
                        let outputs = controller.mapper.release_all();
                        send(
                            *id,
                            controller,
                            self.keys.as_mut(),
                            self.mouse.as_mut(),
                            &outputs,
                        );
                    }
                    self.controllers_off();
                }
            }
        }
    }

    /// Run a `[daemon]` shell command off this thread and report its outcome as `command`'s;
    /// unset has nothing to run, so nothing to fail.
    fn shell(
        &self,
        command: Command,
        line: Option<String>,
    ) {
        let tx_event = self.tx_event.clone();
        let wait = self.wait.clone();
        std::thread::spawn(move || {
            let _wait = wait;
            let outcome = match &line {
                None => Outcome::Done,
                Some(line) => match std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(line)
                    .status()
                {
                    Ok(status) if status.success() => Outcome::Done,
                    Ok(status) => {
                        warn!("{command:?} {line:?}: {status}");
                        Outcome::Failed
                    }
                    Err(err) => {
                        warn!("{command:?} {line:?}: {err}");
                        Outcome::Failed
                    }
                },
            };
            let _ = tx_event.send(Event::Tv(Fact::Outcome(command, outcome)));
        });
    }

    /// Tell the user on every controller.
    fn say(
        &mut self,
        cue: Cue,
    ) {
        info!("{cue:?}");
        for (id, controller) in &mut self.controllers {
            let Some(haptics) = &mut controller.haptics else {
                continue;
            };
            let sent = match cue {
                Cue::Failed => haptics.tone(Side::Right, 220, 300),
            };
            if let Err(err) = sent {
                warn!("{id} haptics: {err}");
            }
        }
    }

    /// Ask the `[daemon]` tv_query command, off this thread, whether the TV is on; if it
    /// exits 0 the engine is told so. Unset or failing tells nothing.
    fn query_tv(&self) {
        let Some(command) = self.config.daemon.tv_query.clone() else {
            return;
        };
        let tx_event = self.tx_event.clone();
        std::thread::spawn(move || {
            match std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .status()
            {
                Ok(status) if status.success() => {
                    info!("TV found on");
                    let fact = Fact::TvPower(TvPower::On, Source::Query);
                    let _ = tx_event.send(Event::Tv(fact));
                }
                Ok(status) => info!("TV query {command:?}: {status}"),
                Err(err) => warn!("TV query {command:?}: {err}"),
            }
        });
    }

    /// Power every controller down, so it sleeps with the TV: the turn-off command on each
    /// node that carries one, and the Bluetooth link dropped for those on it (the command
    /// alone is not known to end the link).
    fn controllers_off(&mut self) {
        let mut bluetooth = false;
        for (id, device) in self.devices.iter().filter(|(_, d)| d.connected) {
            match Hidraw::open(&device.path).and_then(|hidraw| hidraw.power_off()) {
                Ok(()) => info!("{id} powered off"),
                Err(err) => warn!("{id} power off: {err}"),
            }
            bluetooth |= device.link == IpcLink::Bluetooth;
        }
        if bluetooth {
            let tx_event = self.tx_event.clone();
            let wait = self.wait.clone();
            std::thread::spawn(move || {
                let _wait = wait;
                if let Err(err) = bluetooth::disconnect_controllers() {
                    warn!("Bluetooth disconnect: {err}");
                    let fact = Fact::Outcome(Command::ControllersOff, Outcome::Failed);
                    let _ = tx_event.send(Event::Tv(fact));
                }
            });
        }
    }

    /// Hand the controller to its own keyboard and mouse, or take it back, for a `lizard:`
    /// binding. On the way in everything yakin holds is released first; on the way out the
    /// mapper forgets what was pressed meanwhile, as none of it went out.
    fn lizard(
        &mut self,
        id: ControllerId,
        on: bool,
    ) {
        let Some(controller) = self.controllers.get_mut(&id) else {
            return;
        };
        if on {
            let outputs = controller.mapper.release_all();
            send(
                id,
                controller,
                self.keys.as_mut(),
                self.mouse.as_mut(),
                &outputs,
            );
        } else {
            let _ = controller.mapper.release_all();
        }
        if let Some(device) = self.devices.get_mut(&id) {
            device.handed = on;
            if self.config.daemon.lizard_cue
                && let Err(err) = haptics::cue(&device.path, on)
            {
                warn!("{id} cue: {err}");
            }
        }
        self.set_lizard(id, on);
        self.publish(IpcEvent::Controllers(self.status()));
        self.hold();
    }

    /// Switch the firmware's keyboard and mouse on its node; the state is remembered only when
    /// the controller took it.
    fn set_lizard(
        &mut self,
        id: ControllerId,
        on: bool,
    ) {
        let Some(device) = self.devices.get_mut(&id) else {
            return;
        };
        let result = Hidraw::open(&device.path).and_then(|hidraw| {
            if on {
                return hidraw.lizard_on();
            }
            if self.lizard_off {
                hidraw.lizard_off()?;
            }
            // Default settings put the IMU back to the firmware's choice
            if self.config.daemon.imu {
                hidraw.imu_on()?;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                device.lizard = Some(on);
                info!("{id} lizard {}", if on { "on" } else { "off" });
            }
            Err(err) => warn!("{id} lizard: {err}"),
        }
    }

    /// Grab the controller's own keyboard and mouse on every link yakin holds a node of, and
    /// let go of those handed over by a `lizard:` binding or yielded with the controller.
    fn hold(&mut self) {
        let yielded = self.yielded.holder().is_some();
        let devices = &self.devices;
        self.grabs.sync(|phys| {
            let on_link = devices
                .values()
                .filter(|device| device.phys.as_deref() == Some(phys));
            let mut held = false;
            for device in on_link {
                if device.handed {
                    return false;
                }
                held = true;
            }
            held && !yielded
        });
    }

    /// Read the grabbed keyboards and mice; on a link where lizard mode should be off, input
    /// there means the firmware turned it back on: warn, count it and turn it off again.
    fn watch(&mut self) {
        let lizard_off = self.lizard_off;
        let devices = &self.devices;
        let leaks = self.grabs.read(|phys| {
            let on_link: Vec<&Device> = devices
                .values()
                .filter(|device| device.phys.as_deref() == Some(phys))
                .collect();
            lizard_off
                && !on_link.is_empty()
                && on_link
                    .iter()
                    .all(|device| !device.handed && device.lizard == Some(false))
        });
        if leaks.is_empty() {
            return;
        }
        for leak in leaks {
            warn!(
                "lizard leak on {}: {} events while lizard off; sending lizard off",
                leak.phys, leak.events
            );
            let ids: Vec<ControllerId> = self
                .devices
                .iter()
                .filter(|(_, device)| device.phys.as_deref() == Some(leak.phys.as_str()))
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                if let Some(device) = self.devices.get_mut(&id) {
                    device.leaks += 1;
                }
                self.set_lizard(id, false);
            }
        }
        self.publish(IpcEvent::Controllers(self.status()));
    }

    /// Run a `menu:` command, from a binding or a request.
    fn command_menu(
        &mut self,
        command: MenuCommand,
    ) {
        match command.change(self.menu.is_some()) {
            Some(true) => self.open_menu(self.config.menu.clone()),
            Some(false) => self.close_menu(),
            None => {}
        }
    }

    /// Run a `keyboard:` command, from a binding or a request.
    fn command_keyboard(
        &mut self,
        command: MenuCommand,
    ) {
        match command.change(self.keyboard.is_some()) {
            Some(true) => self.open_keyboard(),
            Some(false) => self.close_keyboard(),
            None => {}
        }
    }

    /// Buttons of the active profile bound to close the menu, besides B.
    fn menu_buttons(&self) -> Vec<Button> {
        self.closing(|target| {
            matches!(
                target,
                Target::Menu(MenuCommand::Toggle | MenuCommand::Close)
            )
        })
    }

    /// Buttons of the active profile bound to close the keyboard, besides Steam.
    fn keyboard_buttons(&self) -> Vec<Button> {
        self.closing(|target| {
            matches!(
                target,
                Target::Keyboard(MenuCommand::Toggle | MenuCommand::Close)
            )
        })
    }

    /// Buttons of the active profile whose press `closes`.
    fn closing(
        &self,
        closes: impl Fn(&Target) -> bool,
    ) -> Vec<Button> {
        self.profile
            .buttons
            .iter()
            .filter(|(_, binding)| closes(&binding.press))
            .map(|(button, _)| *button)
            .collect()
    }

    /// Release everything the profile holds, so the menu or keyboard starts from nothing held.
    /// Toggled layers stay on; only a profile switch resets them.
    fn release_profile(&mut self) {
        for (id, controller) in self.controllers.iter_mut() {
            let outputs = controller.mapper.hand_off();
            send(
                *id,
                controller,
                self.keys.as_mut(),
                self.mouse.as_mut(),
                &outputs,
            );
            controller.nav = MenuNav::default();
        }
    }

    /// Hand the controllers to `menu`, focused on its first item.
    fn open_menu(
        &mut self,
        menu: Menu,
    ) {
        self.close_keyboard();
        self.release_profile();
        info!("Menu open");
        self.menu = Some(Menu { focus: 0, ..menu });
        self.publish_menu();
    }

    fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            info!("Menu closed");
            self.publish_menu();
        }
    }

    fn publish_menu(&mut self) {
        self.publish(IpcEvent::Menu(self.menu.as_ref().map(IpcMenu::from)));
    }

    /// Hand the controllers to the on-screen keyboard.
    fn open_keyboard(&mut self) {
        self.close_menu();
        self.release_profile();
        info!("Keyboard open");
        self.keyboard = Some(Keyboard::default());
        self.publish_keyboard();
    }

    fn close_keyboard(&mut self) {
        if self.keyboard.take().is_some() {
            info!("Keyboard closed");
            self.publish_keyboard();
        }
    }

    fn publish_keyboard(&mut self) {
        self.publish(IpcEvent::Keyboard(
            self.keyboard.as_ref().map(IpcKeyboard::from),
        ));
    }

    /// Move the menu's focus, or fire its item as controller `id` and close it; both tick at
    /// `haptics.menu`, a move on the left (D-pad, stick), firing on the right (A, pad, trigger).
    fn navigate(
        &mut self,
        id: ControllerId,
        inputs: Vec<MenuInput>,
    ) {
        let haptics = self.profile.haptics_menu;
        for input in inputs {
            let Some(menu) = &mut self.menu else {
                return;
            };
            match input {
                MenuInput::Move(direction) => {
                    if menu.move_focus(direction) {
                        self.publish_menu();
                        let tick = Output::haptic(Side::Left, haptics, TICK_MS);
                        self.perform(id, &Vec::from_iter(tick));
                    }
                }
                MenuInput::Activate => {
                    let target = menu.activate().cloned();
                    let tick = Output::haptic(Side::Right, haptics, TICK_MS);
                    self.perform(id, &Vec::from_iter(tick));
                    self.close_menu();
                    if let Some(target) = target {
                        info!("Menu {target}");
                        self.perform(id, &tap_outputs(&target));
                    }
                }
                MenuInput::Close => self.close_menu(),
            }
        }
    }

    /// Swap in a new config and re-apply the active profile, `pad` if it is gone.
    fn apply(
        &mut self,
        config: Config,
    ) -> Result<()> {
        for warning in config.warnings() {
            warn!("{warning}");
        }
        let active = match config.get(&self.active) {
            Ok(_) => self.active.clone(),
            Err(_) => PAD.to_string(),
        };
        let profile = config.get(&active)?;
        // The uinput key set is fixed at creation; build the new keyboard first so a failure
        // changes nothing, release through the old one, then replace it. While yielded there
        // is none, reclaiming builds it
        let mut keys = match self.keys {
            Some(_) => Some(Keys::new(config.keys())?),
            None => None,
        };
        self.close_menu();
        self.close_keyboard();
        self.desktop = Control::new(config.desktop.desktop());
        self.tv.configure(config.daemon.tv_settings());
        self.config = config;
        self.selection.reload();
        self.spot();
        self.activate(active, profile);
        std::mem::swap(&mut self.keys, &mut keys);
        self.follow();
        Ok(())
    }

    fn config_file(&self) -> Result<IpcConfig> {
        self.layers.ipc_config()
    }

    fn status(&self) -> Vec<IpcController> {
        self.devices
            .iter()
            .map(|(id, device)| IpcController {
                id: u8::from(*id),
                path: device.path.display().to_string(),
                battery: device.battery,
                wireless: device.wireless,
                link: device.link,
                connected: device.connected,
                lizard: device.lizard,
                leaks: device.leaks,
                mouse: device.handed,
            })
            .collect()
    }

    /// An error while the controller is yielded to another process.
    fn ensure_held(&self) -> Result<()> {
        match self.yielded.holder() {
            Some(holder) => Err(anyhow!("The controller is yielded to {}", holder.comm)),
            None => Ok(()),
        }
    }

    fn holder(&self) -> Option<IpcHolder> {
        self.yielded.holder().map(|holder| IpcHolder {
            pid: holder.pid,
            comm: holder.comm.clone(),
            app: self.game.as_ref().map(Game::to_string),
        })
    }

    /// Look at the focused window for a game Steam Input drives, after a focus change or a
    /// reload. Reads Steam's per-game setting, so not on every tick.
    fn spot(&mut self) {
        let steam = &self.config.steam;
        self.game = Game::of(&self.focus)
            .filter(|game| steam.ignore_steam_input_disabled || !game.steam_input_disabled());
    }

    /// Yield to another process holding the controller while `[steam] yield` says so, or
    /// reclaim it once none does or the reason is gone.
    fn check(&mut self) {
        let wanted = match self.config.steam.yielding {
            Yielding::Always => true,
            Yielding::Games => self.game.is_some(),
            Yielding::Never => false,
        };
        let paths: Vec<PathBuf> = match self.yielded.holder() {
            Some(_) => hidraw::scan(),
            None => self
                .devices
                .values()
                .map(|device| device.path.clone())
                .collect(),
        };
        let holders = holders::holders(Path::new(holders::PROC), &paths);
        match self.yielded.step(&holders, wanted) {
            Step::Stay => {}
            Step::Yield(holder) => self.step_aside(holder),
            Step::Reclaim => {
                let why = match holders.first() {
                    Some(holder) => format!("{} holds it, nothing to yield for", holder.comm),
                    None => "nobody holds it".to_string(),
                };
                match self.reclaim() {
                    Ok(()) => info!("Reclaimed the controller: {why}"),
                    Err(err) => warn!("Reclaim: {err}"),
                }
            }
        }
    }

    /// Release everything, drop the virtual devices and close the controller's nodes.
    /// Steam wraps a virtual pad as a controller of its own, so closing the nodes is not enough.
    fn step_aside(
        &mut self,
        holder: Holder,
    ) {
        match &self.game {
            Some(game) => warn!(
                "Yielding to {} (pid {}): {game} focused",
                holder.comm, holder.pid
            ),
            None => warn!("Yielding to {} (pid {})", holder.comm, holder.pid),
        }
        self.close_menu();
        self.close_keyboard();
        let ids: Vec<ControllerId> = self.controllers.keys().copied().collect();
        for id in ids {
            self.disconnect(id);
        }
        self.keys = None;
        self.mouse = None;
        self.pause.set(true);
        self.yielded.yielded(holder);
        self.hold();
        self.publish(IpcEvent::Yielded(self.holder()));
    }

    /// Recreate the keyboard and mouse and let the readers reopen the nodes; they turn lizard
    /// mode off again and the pads come back with the active profile on their first report.
    fn reclaim(&mut self) -> Result<()> {
        let keys = Keys::new(self.config.keys())?;
        let mouse = Mouse::new()?;
        self.keys = Some(keys);
        self.mouse = Some(mouse);
        self.pause.set(false);
        self.yielded.reclaimed();
        self.publish(IpcEvent::Yielded(None));
        self.follow();
        Ok(())
    }

    fn publish(
        &mut self,
        event: IpcEvent,
    ) {
        self.subscribers
            .retain(|subscriber| !subscriber.is_closed());
        for subscriber in &self.subscribers {
            subscriber.event(event.clone());
        }
    }

    /// Every profile name, sorted, the built-in [`PAD`] included.
    fn profiles(&self) -> Vec<String> {
        self.config
            .profiles
            .keys()
            .map(String::as_str)
            .chain([PAD])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// A profile binding released on the controller: like Activate, a no-op on the active one.
    fn switch(
        &mut self,
        switch: &ProfileSwitch,
    ) {
        let profiles = self.profiles();
        let Some(name) = switch.pick(&self.active, &profiles) else {
            return;
        };
        let name = name.to_string();
        self.hold_pick(&name);
        if name == self.active {
            return;
        }
        match self.config.get(&name) {
            Ok(profile) => self.activate(name, profile),
            Err(err) => warn!("{err}"),
        }
    }

    /// Remember a switch by hand for the selected app, even with an ignored one focused.
    fn hold_pick(
        &mut self,
        name: &str,
    ) {
        self.selection.hold(name);
    }

    /// Switch to the profile picked by hand for the selected app, else the one `[auto]` picks,
    /// unless it is off or the controller is yielded.
    fn follow(&mut self) {
        let auto = &self.config.auto;
        if !auto.enabled || self.yielded.holder().is_some() {
            return;
        }
        let Some(name) = self.selection.pick(auto) else {
            return;
        };
        if name == self.active {
            return;
        }
        let name = name.to_string();
        match self.config.get(&name) {
            Ok(profile) => {
                let app_id = self.selection.app().map_or("", |app| app.app_id.as_str());
                info!("Focus {app_id} picks {name}");
                self.activate(name, profile);
            }
            Err(err) => warn!("{err}"),
        }
    }

    fn auto(&self) -> Option<IpcAuto> {
        let auto = &self.config.auto;
        if !auto.enabled {
            return None;
        }
        let app = self.selection.app().cloned().unwrap_or_default();
        Some(IpcAuto {
            rule: auto.rule(&app.app_id, self.selection.title()),
            profile: auto_profile(auto, &app.app_id, self.selection.title()).map(String::from),
            manual: self.selection.held(),
            focused_app_id: app.app_id,
        })
    }

    /// Release what every controller holds under the old profile, then swap it in.
    fn activate(
        &mut self,
        name: String,
        profile: Profile,
    ) {
        for (id, controller) in self.controllers.iter_mut() {
            let outputs = controller.mapper.activate(profile.clone());
            send(
                *id,
                controller,
                self.keys.as_mut(),
                self.mouse.as_mut(),
                &outputs,
            );
        }
        info!("Profile {name}");
        self.active = name;
        self.profile = profile;
        self.publish(IpcEvent::Profile(self.active.clone()));
    }

    /// When the next tuning settles, if any is pending.
    pub fn deadline(&self) -> Option<Instant> {
        let tuned = self.unsaved.values().min().map(|tuned| *tuned + SETTLE);
        [tuned, self.tv.deadline()].into_iter().flatten().min()
    }

    /// The machine suspends or resumed: the TV's owner decides what that asks. Returns a
    /// channel that disconnects when every command it started has finished.
    fn sleep(
        &mut self,
        phase: SleepPhase,
    ) -> Receiver<()> {
        info!("Sleep {phase:?}");
        let (tx_wait, rx_wait) = crossbeam_channel::unbounded();
        self.wait = Some(tx_wait);
        let commands = self.tv.fact(&Fact::Sleep(phase), Instant::now());
        self.run(None, commands);
        self.wait = None;
        rx_wait
    }

    /// Ask again about what the TV's power is believed to be, once the belief is old.
    pub fn tick_tv(
        &mut self,
        now: Instant,
    ) {
        let commands = self.tv.tick(now);
        self.run(None, commands);
    }

    /// Write the knobs of every profile whose tuning rested [`SETTLE`] by `now`.
    pub fn save_settled(
        &mut self,
        now: Instant,
    ) {
        let settled: Vec<String> = self
            .unsaved
            .iter()
            .filter(|(_, tuned)| **tuned + SETTLE <= now)
            .map(|(name, _)| name.clone())
            .collect();
        self.save_tunings(settled);
    }

    /// Write every pending tuning now, settled or not.
    pub fn flush(&mut self) {
        let names = self.unsaved.keys().cloned().collect();
        self.save_tunings(names);
    }

    /// Write the knobs of `names` to the editor layer. A failure is logged once and dropped:
    /// the tuning still runs in memory.
    fn save_tunings(
        &mut self,
        names: Vec<String>,
    ) {
        if names.is_empty() {
            return;
        }
        for name in names.iter() {
            self.unsaved.remove(name);
        }
        // A reload may have dropped a profile since
        let tunings: Vec<IpcTuning> = names
            .iter()
            .filter_map(|name| Some(tuning(name, &self.config.get(name).ok()?)))
            .collect();
        match self.layers.save_tuning(&tunings) {
            Ok(_) => {
                self.save_failed = false;
                match self.config_file() {
                    Ok(config) => self.publish(IpcEvent::Config(Box::new(config))),
                    Err(err) => warn!("Cannot read the config back: {err}"),
                }
            }
            Err(err) => {
                if !self.save_failed {
                    warn!("Cannot save tuning: {err}");
                }
                self.save_failed = true;
            }
        }
    }

    /// Change a profile's knobs in memory at once; [`Engine::save_settled`] writes them later.
    fn tune(
        &mut self,
        tuning: &IpcTuning,
    ) -> Result<()> {
        let values = [
            tuning.mouse_speed,
            tuning.mouse_accel,
            tuning.scroll_speed,
            tuning.mouse_stick_speed,
            tuning.mouse_flick_degrees,
            tuning.scroll_stick_speed,
            tuning.deadzone_stick,
            tuning.deadzone_trigger,
            tuning.trigger_click,
        ];
        if values
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err(anyhow!("Tuning values must be finite and not negative"));
        }
        if tuning.mouse_release_guard > RELEASE_GUARD_MAX {
            return Err(anyhow!("mouse_release_guard is over {RELEASE_GUARD_MAX}"));
        }
        if !(0.0..=1.0).contains(&tuning.mouse_smoothing) {
            return Err(anyhow!("mouse_smoothing is not within 0 to 1"));
        }
        if !(1.0..=3.0).contains(&tuning.mouse_stick_curve) {
            return Err(anyhow!("mouse_stick_curve is not within 1 to 3"));
        }
        for (name, value) in [
            ("haptics_click", tuning.haptics_click),
            ("haptics_pad_ticks", tuning.haptics_pad_ticks),
            ("haptics_menu", tuning.haptics_menu),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(anyhow!("{name} is not within 0 to 1"));
            }
        }
        let mut profile = self.config.get(&tuning.profile)?;
        profile.mouse_speed = tuning.mouse_speed;
        profile.mouse_accel = tuning.mouse_accel;
        profile.mouse_release_guard = tuning.mouse_release_guard;
        profile.mouse_smoothing = tuning.mouse_smoothing;
        profile.mouse_stick_speed = tuning.mouse_stick_speed;
        profile.mouse_stick_curve = tuning.mouse_stick_curve;
        profile.mouse_flick_degrees = tuning.mouse_flick_degrees;
        profile.scroll_speed = tuning.scroll_speed;
        profile.scroll_natural = tuning.scroll_natural;
        profile.scroll_stick_speed = tuning.scroll_stick_speed;
        profile.deadzone_stick = tuning.deadzone_stick;
        profile.deadzone_trigger = tuning.deadzone_trigger;
        profile.trigger_click = tuning.trigger_click;
        profile.haptics_click = tuning.haptics_click;
        profile.haptics_pad_ticks = tuning.haptics_pad_ticks;
        profile.haptics_menu = tuning.haptics_menu;
        if tuning.profile == self.active {
            for controller in self.controllers.values_mut() {
                controller.mapper.retune(profile.clone());
            }
            self.profile = profile.clone();
        }
        self.config.profiles.insert(tuning.profile.clone(), profile);
        Ok(())
    }

    /// Drop the virtual pad, releasing any key it still holds.
    fn disconnect(
        &mut self,
        id: ControllerId,
    ) {
        let Some(mut controller) = self.controllers.remove(&id) else {
            return;
        };
        let outputs = controller.mapper.release_all();
        send(
            id,
            &mut controller,
            self.keys.as_mut(),
            self.mouse.as_mut(),
            &outputs,
        );
        info!("{id} disconnected");
        // A binding's release that never came: take the keyboard and mouse back, and if the
        // link is gone already, on its next report
        let handed = self.devices.get(&id).is_some_and(|device| device.handed);
        if handed {
            if let Some(device) = self.devices.get_mut(&id) {
                device.handed = false;
                device.lizard = Some(true);
            }
            if self.lizard_off {
                self.set_lizard(id, false);
            }
            self.hold();
        }
    }
}

pub fn tuning(
    name: &str,
    profile: &Profile,
) -> IpcTuning {
    IpcTuning {
        profile: name.to_string(),
        mouse_speed: profile.mouse_speed,
        mouse_accel: profile.mouse_accel,
        mouse_release_guard: profile.mouse_release_guard,
        mouse_smoothing: profile.mouse_smoothing,
        mouse_stick_speed: profile.mouse_stick_speed,
        mouse_stick_curve: profile.mouse_stick_curve,
        mouse_flick_degrees: profile.mouse_flick_degrees,
        scroll_speed: profile.scroll_speed,
        scroll_natural: profile.scroll_natural,
        scroll_stick_speed: profile.scroll_stick_speed,
        deadzone_stick: profile.deadzone_stick,
        deadzone_trigger: profile.deadzone_trigger,
        trigger_click: profile.trigger_click,
        haptics_click: profile.haptics_click,
        haptics_pad_ticks: profile.haptics_pad_ticks,
        haptics_menu: profile.haptics_menu,
    }
}

/// What `id` says now, as a duration; zero if it cannot be read.
fn clock(id: ClockId) -> Duration {
    clock_gettime(id).map(Duration::from).unwrap_or_default()
}

fn send(
    id: ControllerId,
    controller: &mut Controller,
    keys: Option<&mut Keys>,
    mouse: Option<&mut Mouse>,
    outputs: &[Output],
) {
    if let Err(err) = controller.pad.send(outputs) {
        warn!("{id} pad: {err}");
    }
    let now = Instant::now();
    for output in outputs {
        let Some(haptics) = &mut controller.haptics else {
            break;
        };
        let sent = match *output {
            Output::Haptic { side, strength, ms } if controller.limiter.allow(side, now) => {
                haptics.pulse(side, strength, ms)
            }
            Output::Tone { side, hz, ms } if controller.limiter.allow(side, now) => {
                haptics.tone(side, hz, ms)
            }
            _ => Ok(()),
        };
        if let Err(err) = sent {
            warn!("{id} haptics: {err}");
        }
    }
    if let Some(keys) = keys
        && let Err(err) = keys.send(outputs)
    {
        warn!("{id} keys: {err}");
    }
    if let Some(mouse) = mouse
        && let Err(err) = mouse.send(outputs)
    {
        warn!("{id} mouse: {err}");
    }
}
