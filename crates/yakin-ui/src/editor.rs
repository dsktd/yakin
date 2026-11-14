//! The binding editor: the draft, the profile and layer being edited, and the page that shows
//! the profile's layout, gyro, bindings and chords, with the layer, menu and rule sections below.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use dioxus::prelude::*;
use yakin_core::profile::BindingToml;
use yakin_core::profile::ChordToml;
use yakin_core::profile::ConfigToml;
use yakin_core::profile::ProfileToml;
use yakin_core::profile::RemoveToml;
use yakin_core::profile::Vocabulary;
use yakin_core::state::ControllerState;
use yakin_ipc::IpcConfig;

use crate::components::button::Button;
use crate::components::button::ButtonSize;
use crate::components::button::ButtonVariant;
use crate::components::input::Input;
use crate::components::slider::Slider;
use crate::daemon::Daemon;
use crate::draft;
use crate::draft::Group;
use crate::draft::Kinds;
use crate::layers::LayerSection;
use crate::live::Live;
use crate::menu::MenuSection;
use crate::rules::RuleSection;
use crate::target::Pick;
use crate::target::TargetCell;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Editor {
    pub vocab: Option<Vocabulary>,
    pub file: Option<IpcConfig>,
    pub draft: ConfigToml,
    pub editing: Option<String>,
    /// The layer picked in the Layers section; reset when the profile has no such layer.
    pub layer: Option<String>,
    /// Paths (buttons.a, chord.0.b) whose activator disclosure is open, kept across renders.
    pub opened: BTreeSet<String>,
    /// A save error next to the row it names, by path.
    pub row_errors: BTreeMap<String, String>,
    /// Any other error, above the table.
    pub error: String,
}
impl Editor {
    /// The vocabulary and config of the daemon; an old daemon cannot answer, which shows.
    pub fn load(
        &mut self,
        daemon: &Daemon,
    ) {
        match daemon
            .vocabulary()
            .and_then(|vocab| Ok((vocab, daemon.config()?)))
        {
            Ok((vocab, config)) => {
                self.vocab = Some(vocab);
                self.set_file(config, false);
            }
            Err(err) => self.show_error(&err),
        }
    }

    pub fn set_file(
        &mut self,
        config: IpcConfig,
        replace: bool,
    ) {
        let keep = self.file.is_some() && self.dirty() && !replace;
        if !keep {
            self.draft = config.effective.clone();
        }
        self.file = Some(config);
        self.row_errors.clear();
        self.error.clear();
    }

    pub fn dirty(&self) -> bool {
        self.file
            .as_ref()
            .is_some_and(|file| file.effective != self.draft)
    }

    /// The profile being edited, `pad` when the picked one is gone.
    pub fn editing(&self) -> String {
        let names = draft::profile_names(&self.draft);
        match &self.editing {
            Some(name) if names.contains(name) => name.clone(),
            _ => "pad".to_string(),
        }
    }

    /// The profile being edited, created empty on first change: an absent profile means defaults.
    pub fn profile(&mut self) -> &mut ProfileToml {
        let name = self.editing();
        self.draft.profile.entry(name).or_default()
    }

    pub fn current(&self) -> Option<&ProfileToml> {
        self.draft.profile.get(&self.editing())
    }

    pub fn layer_names(&self) -> Vec<String> {
        self.current()
            .map(|profile| profile.layer.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn kinds(&self) -> Kinds {
        Kinds::new(self.vocab.as_ref().unwrap(), &self.layer_names())
    }

    pub fn show_error(
        &mut self,
        err: &str,
    ) {
        let placed = draft::place(err);
        if let Some(profile) = placed.profile
            && draft::profile_names(&self.draft).contains(&profile)
        {
            self.editing = Some(profile);
        }
        match placed.path {
            Some(path) => _ = self.row_errors.insert(path, placed.text),
            None => self.error = placed.text,
        }
    }

    pub fn save(
        &mut self,
        daemon: &Daemon,
    ) {
        match daemon
            .set_config(self.draft.clone())
            .and_then(|()| daemon.config())
        {
            Ok(config) => self.set_file(config, true),
            Err(err) => self.show_error(&err),
        }
    }

    pub fn revert(
        &mut self,
        daemon: &Daemon,
    ) {
        match daemon.config() {
            Ok(config) => self.set_file(config, true),
            Err(err) => self.show_error(&err),
        }
    }

    /// A new profile, a copy of the one being edited.
    pub fn new_profile(
        &mut self,
        name: &str,
    ) {
        let copy = self.current().cloned().unwrap_or_default();
        self.add_profile(name, copy);
    }

    /// A new profile based on the one being edited, with no overrides yet.
    pub fn fork_profile(
        &mut self,
        name: &str,
    ) {
        let fork = ProfileToml {
            base: Some(self.editing()),
            ..Default::default()
        };
        self.add_profile(name, fork);
    }

    fn add_profile(
        &mut self,
        name: &str,
        profile: ProfileToml,
    ) {
        if name.is_empty() || draft::profile_names(&self.draft).contains(&name.to_string()) {
            let err = if name.is_empty() {
                "Name the new profile first".to_string()
            } else {
                format!("Profile {name} exists")
            };
            return self.show_error(&err);
        }
        self.draft.profile.insert(name.to_string(), profile);
        self.editing = Some(name.to_string());
    }

    pub fn deletable(&self) -> bool {
        self.file
            .as_ref()
            .is_some_and(|file| draft::deletable(file, &self.editing()))
    }

    pub fn delete_profile(&mut self) {
        if !self.deletable() {
            return;
        }
        let name = self.editing();
        self.draft.profile.remove(&name);
        if self.draft.daemon.active.as_deref() == Some(name.as_str()) {
            self.draft.daemon.active = Some("pad".to_string());
        }
        self.editing = Some("pad".to_string());
    }
}

/// The directory part of a path.
fn dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(at) => &path[..at],
    }
}

#[component]
pub fn EditorPage(
    editor: Signal<Editor>,
    live: Signal<Live>,
    state: Signal<ControllerState>,
) -> Element {
    let daemon = use_context::<Daemon>();
    let mut name = use_signal(String::new);
    let buttons = use_memo(move || {
        editor
            .read()
            .vocab
            .as_ref()
            .map(|vocab| vocab.buttons.clone())
            .unwrap_or_default()
    });
    // Only a change in what is lit re-renders the rows
    let lit = use_memo(move || crate::live::lit(&state.read(), &buttons.read()));
    let guard = editor.read();
    let (Some(vocab), Some(file)) = (&guard.vocab, &guard.file) else {
        let error = guard.error.clone();
        return rsx! {
            div { class: "editor",
                p { class: "note", "No config from the daemon yet." }
                p { class: "row-error", {error} }
            }
        };
    };
    let editing = guard.editing();
    let profiles = draft::profile_names(&guard.draft);
    let changed = guard.dirty();
    let where_ = format!(
        "{}, drop-ins in {}{}",
        file.baseline
            .as_ref()
            .map_or("no baseline", |layer| layer.path.as_str()),
        dir(&file.editor_path),
        if file.writable { "" } else { " (read-only)" }
    );
    let layout = guard
        .current()
        .and_then(|p| p.layout.clone())
        .unwrap_or_default();
    let layouts: Vec<(String, String)> =
        std::iter::once(("".to_string(), "default (xbox)".to_string()))
            .chain(vocab.layouts.iter().map(|l| (l.clone(), l.clone())))
            .collect();
    let gyro = guard.current().map(|p| p.gyro.clone()).unwrap_or_default();
    let gyro_buttons: Vec<(String, String)> =
        std::iter::once(("".to_string(), "always on".to_string()))
            .chain(vocab.buttons.iter().map(|b| (b.clone(), b.clone())))
            .collect();
    let chords = guard.current().map(|p| p.chord.len()).unwrap_or(0);
    let deletable = guard.deletable();
    let based_on = guard.current().and_then(|profile| profile.base.clone());
    let error = guard.error.clone();
    let sources: Vec<(Group, String)> = vocab
        .buttons
        .iter()
        .map(|s| (Group::Buttons, s.clone()))
        .chain(vocab.axes.iter().map(|s| (Group::Axes, s.clone())))
        .collect();
    drop(guard);
    let daemon_ = daemon.clone();
    let daemon_save = daemon.clone();
    rsx! {
        div { class: "editor",
            div { class: "bar",
                Pick {
                    options: profiles.iter().map(|n| (n.clone(), n.clone())).collect::<Vec<_>>(),
                    value: editing.clone(),
                    onchange: move |n: String| editor.write().editing = Some(n),
                }
                Input {
                    placeholder: "new name",

                    value: name(),
                    oninput: move |e: FormEvent| name.set(e.value()),
                }
                Button { variant: ButtonVariant::Outline, size: ButtonSize::Sm,
                    onclick: move |_| editor.write().new_profile(name().trim()),
                    "New profile"
                }
                Button { variant: ButtonVariant::Outline, size: ButtonSize::Sm,
                    title: "A new profile based on this one, holding only what you change",
                    onclick: move |_| editor.write().fork_profile(name().trim()),
                    "Fork profile"
                }
                Button { variant: ButtonVariant::Outline, size: ButtonSize::Sm, disabled: !deletable,
                    onclick: move |_| editor.write().delete_profile(),
                    "Delete profile"
                }
                span { class: "spacer" }
                span { class: "note", if changed { "unsaved" } }
                Button { variant: ButtonVariant::Outline, size: ButtonSize::Sm,
                    onclick: move |_| editor.write().revert(&daemon_),
                    "Revert"
                }
                Button { size: ButtonSize::Sm, disabled: !changed,
                    onclick: move |_| editor.write().save(&daemon_save),
                    "Save"
                }
            }
            p { class: "note", {where_} }
            if let Some(base) = based_on {
                p { class: "note", {format!("Based on {base}: unset values come from it")} }
            }
            p { class: "row-error", {error} }
            p { class: "inline",
                "Layout "
                Pick { options: layouts, value: layout,
                    onchange: move |value: String| {
                        let mut editor = editor.write();
                        editor.profile().layout = (!value.is_empty()).then_some(value);
                    }
                }
            }
            // Gyro settings of the profile: the button that enables it and its speed. The gyro
            // axis itself is bound like any other (cursor or a pad stick); the daemon needs daemon.imu
            p { class: "inline gyro",
                "Gyro enabled by "
                Pick { options: gyro_buttons, value: gyro.button.clone().unwrap_or_default(),
                    onchange: move |value: String| {
                        let mut editor = editor.write();
                        editor.profile().gyro.button = (!value.is_empty()).then_some(value);
                    }
                }
                " speed "
                Slider { value: Some(gyro.speed.unwrap_or(1.0) as f64), min: 0.1, max: 4.0, step: 0.1,
                    label: Some("Gyro speed".to_string()),
                    on_value_change: move |value: f64| {
                        let mut editor = editor.write();
                        let speed = (value * 10.0).round() as f32 / 10.0;
                        editor.profile().gyro.speed = (speed != 1.0).then_some(speed);
                    }
                }
                span { class: "note", {format!("{}", gyro.speed.unwrap_or(1.0))} }
            }
            h2 { "Bindings" }
            table { class: "bindings",
                for (group, source) in sources {
                    BindingRow { key: "{source}", editor, group, source: source.clone(), layer: None,
                        lit: lit.read().contains(&source) }
                }
            }
            h2 { "Chords" }
            BaseChords { editor }
            for index in 0..chords {
                ChordBlock { key: "{index}", editor, index, lit: lit.read().clone() }
            }
            Button { variant: ButtonVariant::Outline, size: ButtonSize::Sm,
                onclick: move |_| editor.write().profile().chord.push(ChordToml { hold: "view".to_string(), remove: None, buttons: BTreeMap::new() }),
                "Add chord"
            }
            LayerSection { editor, lit: lit.read().clone() }
            MenuSection { editor }
            RuleSection { editor, live }
        }
    }
}

/// One source's binding: its target, the activators behind "more", the layer tag and reset.
#[component]
pub fn BindingRow(
    editor: Signal<Editor>,
    group: Group,
    source: String,
    layer: Option<String>,
    lit: bool,
    /// The target's kind select takes the focus when the row mounts.
    #[props(default)]
    focus: bool,
) -> Element {
    let guard = editor.read();
    let vocab = guard.vocab.as_ref().unwrap();
    let file = guard.file.as_ref().unwrap();
    let editing = guard.editing();
    let mine_value = guard
        .current()
        .and_then(|profile| draft::binding(profile, layer.as_deref(), group, &source));
    let (under, tag) = draft::under_binding(file, &editing, layer.as_deref(), group, &source);
    let mine = mine_value != under;
    let inherited = draft::base_binding(&guard.draft, &editing, layer.as_deref(), group, &source);
    let base = inherited.as_ref().map(|(_, base)| base.clone());
    let unset = match (&inherited, &layer) {
        (Some((binding, base)), _) => {
            format!("from {base} ({})", binding.press().unwrap_or("none"))
        }
        (None, Some(_)) => "profile's".to_string(),
        (None, None) => format!(
            "default ({})",
            vocab.defaults.get(&source).map_or("none", String::as_str)
        ),
    };
    let overrides = base.is_some() && mine_value.is_some();
    let path = match &layer {
        Some(name) => format!("layer.{name}.{}.{source}", group.name()),
        None => format!("{}.{source}", group.name()),
    };
    let row_error = guard.row_errors.get(&path).cloned().unwrap_or_default();
    drop(guard);
    let source_ = source.clone();
    let layer_ = layer.clone();
    let set = Callback::new(move |binding: Option<BindingToml>| {
        let mut editor = editor.write();
        draft::set_binding(
            editor.profile(),
            layer_.as_deref(),
            group,
            &source_,
            binding,
        );
    });
    let reset_to = under.clone();
    rsx! {
        tr { "data-source": source.clone(), class: if lit { "pressed" },
            th { {source.clone()} }
            if group == Group::Buttons {
                Activators { editor, source: source.clone(), binding: mine_value, set, unset: Some(unset), path: path.clone(), focus }
            } else {
                TargetCell { editor, source: source.clone(),
                    target: mine_value.as_ref().and_then(|b| b.press().map(str::to_string)),
                    set: move |target: Option<String>| set.call(target.map(BindingToml::Target)),
                    unset: Some(unset), skip: Vec::new(), focus }
                td {}
            }
            td {
                span { class: "tag", if mine { "yours" } else { {tag} } }
                if mine {
                    Button { variant: ButtonVariant::Ghost, size: ButtonSize::IconXs, title: "Reset to baseline",
                        onclick: move |_| set.call(reset_to.clone()),
                        "↺"
                    }
                }
                if overrides {
                    Button { variant: ButtonVariant::Ghost, size: ButtonSize::IconXs,
                        title: format!("Reset to base ({})", base.clone().unwrap_or_default()),
                        onclick: move |_| set.call(None),
                        "⤓"
                    }
                }
            }
            td { span { class: "row-error", {row_error} } }
        }
    }
}

/// The press target's cell of a button binding plus a "more…" disclosure with the other
/// activators; `set` gets the packed binding.
#[component]
pub fn Activators(
    editor: Signal<Editor>,
    source: String,
    binding: Option<BindingToml>,
    set: Callback<Option<BindingToml>>,
    unset: Option<String>,
    path: String,
    /// The press target's kind select takes the focus when it mounts.
    #[props(default)]
    focus: bool,
) -> Element {
    let slots = draft::slots(binding.as_ref());
    let open = editor.read().opened.contains(&path);
    let extra = draft::extra(&slots);
    let update = {
        let slots = slots.clone();
        move |name: &str, target: Option<String>| {
            let mut next = slots.clone();
            draft::set_slot(&mut next, name, target);
            set.call(draft::pack(next));
        }
    };
    let update_press = update.clone();
    let path_ = path.clone();
    rsx! {
        TargetCell { editor, source: source.clone(), target: slots.press.clone(),
            set: move |target: Option<String>| update_press("press", target), unset, skip: Vec::new(), focus }
        td {
            Button { variant: ButtonVariant::Ghost, size: ButtonSize::Xs, "aria-expanded": open,
                onclick: move |()| {
                    let mut editor = editor.write();
                    if editor.opened.contains(&path_) {
                        editor.opened.remove(&path_);
                    } else {
                        editor.opened.insert(path_.clone());
                    }
                },
                if extra > 0 { "more ({extra})" } else { "more…" }
            }
            if open {
                table { class: "activators",
                    for name in draft::SLOTS {
                        tr { key: "{name}",
                            th { {name} }
                            TargetCell { editor, source: source.clone(),
                                target: draft::slot(&slots, name).map(str::to_string),
                                set: {
                                    let update = update.clone();
                                    move |target: Option<String>| update(name, target)
                                },
                                unset: Some("none".to_string()), skip: Vec::new() }
                        }
                    }
                    for (knob, fallback) in draft::TIMINGS {
                        tr { key: "{knob}",
                            th { {knob} }
                            td {
                                Input { r#type: "number", min: "1", placeholder: fallback,
                                    value: draft::timing(&slots, knob).map(|ms| ms.to_string()).unwrap_or_default(),
                                    onchange: {
                                        let slots = slots.clone();
                                        move |e: FormEvent| {
                                            let mut next = slots.clone();
                                            let ms = e.value().trim().parse::<u32>().ok().filter(|ms| *ms > 0);
                                            draft::set_timing(&mut next, knob, ms);
                                            set.call(draft::pack(next));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The base's chords on holds this profile leaves alone, read only, each with a way to
/// change it (an own chord on that hold, merged into the base's) or drop it.
#[component]
fn BaseChords(editor: Signal<Editor>) -> Element {
    let guard = editor.read();
    let editing = guard.editing();
    let Some((chords, base)) = draft::base_chords(&guard.draft, &editing) else {
        return rsx! {};
    };
    let own: Vec<String> = guard
        .current()
        .map(|p| p.chord.iter().map(|c| c.hold.clone()).collect())
        .unwrap_or_default();
    drop(guard);
    rsx! {
        for chord in chords.into_iter().filter(|chord| !own.contains(&chord.hold)) {
            div { class: "chord", key: "base-{chord.hold}",
                div { class: "chord-head",
                    {format!("Hold {} (from {base}) ", chord.hold)}
                    Button { variant: ButtonVariant::Outline, size: ButtonSize::Sm,
                        title: "Add your own members, merged into the base's",
                        onclick: { let hold = chord.hold.clone(); move |_| push_chord(editor, hold.clone(), None) },
                        "Change"
                    }
                    Button { variant: ButtonVariant::Outline, size: ButtonSize::Sm,
                        onclick: { let hold = chord.hold.clone(); move |_| push_chord(editor, hold.clone(), Some(RemoveToml::Chord(true))) },
                        "Drop"
                    }
                }
                p { class: "note",
                    {chord.buttons.iter().map(|(source, binding)| format!("{source} → {}", binding.press().unwrap_or("none"))).collect::<Vec<_>>().join(", ")}
                }
            }
        }
    }
}

fn push_chord(
    mut editor: Signal<Editor>,
    hold: String,
    remove: Option<RemoveToml>,
) {
    let chord = ChordToml {
        hold,
        remove,
        buttons: BTreeMap::new(),
    };
    editor.write().profile().chord.push(chord);
}

#[component]
fn ChordBlock(
    editor: Signal<Editor>,
    index: usize,
    lit: Vec<String>,
) -> Element {
    let guard = editor.read();
    let vocab = guard.vocab.as_ref().unwrap();
    let Some(chord) = guard.current().and_then(|p| p.chord.get(index)).cloned() else {
        return rsx! {};
    };
    let path = format!("chord.{index}");
    let base = draft::base_chords(&guard.draft, &guard.editing()).and_then(|(chords, base)| {
        let chord = chords.into_iter().find(|base| base.hold == chord.hold)?;
        Some((chord, base))
    });
    let dropped = chord.remove == Some(RemoveToml::Chord(true));
    // The base's members this chord leaves unset or drops, shown under its own
    let inherited: Vec<(String, BindingToml, bool)> = base
        .iter()
        .flat_map(|(base, _)| &base.buttons)
        .filter(|(source, _)| !chord.buttons.contains_key(*source))
        .map(|(source, binding)| {
            (
                source.clone(),
                binding.clone(),
                draft::removes(&chord, source),
            )
        })
        .collect();
    let base_name = base
        .as_ref()
        .map(|(_, name)| name.clone())
        .unwrap_or_default();
    let base_members = base.map(|(chord, _)| chord.buttons).unwrap_or_default();
    let buttons: Vec<(String, String)> = vocab
        .buttons
        .iter()
        .map(|b| (b.clone(), b.clone()))
        .collect();
    let free: Vec<(String, String)> = std::iter::once(("".to_string(), "add button…".to_string()))
        .chain(
            vocab
                .buttons
                .iter()
                .filter(|name| **name != chord.hold && !chord.buttons.contains_key(*name))
                .map(|b| (b.clone(), b.clone())),
        )
        .collect();
    let hold_error = guard
        .row_errors
        .get(&format!("{path}.hold"))
        .cloned()
        .unwrap_or_default();
    let errors = guard.row_errors.clone();
    drop(guard);
    rsx! {
        div { class: "chord",
            div { class: "chord-head",
                "Hold "
                Pick { options: buttons, value: chord.hold.clone(),
                    onchange: move |hold: String| editor.write().profile().chord[index].hold = hold }
                Button { variant: ButtonVariant::Outline, size: ButtonSize::Sm,
                    title: if base_name.is_empty() { "" } else { "Drop your changes; the base's chord stays" },
                    onclick: move |_| _ = editor.write().profile().chord.remove(index),
                    if base_name.is_empty() { "Remove chord" } else { "Reset to base" }
                }
                span { class: "row-error", {hold_error} }
            }
            if dropped {
                p { class: "note", {format!("Drops {base_name}'s chord on this hold")} }
            }
            table {
                for (source, binding) in chord.buttons.iter() {
                    {
                        let source_ = source.clone();
                        let source_remove = source.clone();
                        let row_path = format!("{path}.{source}");
                        let row_error = errors.get(&row_path).cloned().unwrap_or_default();
                        rsx! {
                            tr { key: "{source}", "data-source": source.clone(), class: if lit.contains(source) { "pressed" },
                                th { {source.clone()} }
                                Activators { editor, source: source.clone(), binding: Some(binding.clone()),
                                    set: move |binding: Option<BindingToml>| {
                                        let binding = binding.unwrap_or_else(|| BindingToml::Target("none".to_string()));
                                        editor.write().profile().chord[index].buttons.insert(source_.clone(), binding);
                                    },
                                    unset: None, path: row_path.clone() }
                                td { span { class: "row-error", {row_error} } }
                                td {
                                    if let Some(from) = base_members.get(source) {
                                        Button { variant: ButtonVariant::Ghost, size: ButtonSize::IconXs,
                                            title: format!("Reset to base ({base_name}: {})", from.press().unwrap_or("none")),
                                            onclick: move |_| _ = editor.write().profile().chord[index].buttons.remove(&source_remove),
                                            "⤓"
                                        }
                                    } else {
                                        Button { variant: ButtonVariant::Ghost, size: ButtonSize::IconXs, title: "Remove",
                                            onclick: move |_| _ = editor.write().profile().chord[index].buttons.remove(&source_remove),
                                            "−"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for (source, binding, removed) in inherited {
                    {
                        let source_set = source.clone();
                        let source_drop = source.clone();
                        let from = if removed {
                            format!("dropped from {base_name}")
                        } else {
                            format!("from {base_name} ({})", binding.press().unwrap_or("none"))
                        };
                        rsx! {
                            tr { key: "base-{source}", "data-source": source.clone(), class: if lit.contains(&source) { "pressed" },
                                th { {source.clone()} }
                                Activators { editor, source: source.clone(), binding: None,
                                    set: move |binding: Option<BindingToml>| {
                                        if let Some(binding) = binding {
                                            let mut editor = editor.write();
                                            let chord = &mut editor.profile().chord[index];
                                            draft::remove_member(chord, &source_set, false);
                                            chord.buttons.insert(source_set.clone(), binding);
                                        }
                                    },
                                    unset: Some(from), path: format!("{path}.{source}") }
                                td {}
                                td {
                                    Button { variant: ButtonVariant::Ghost, size: ButtonSize::IconXs,
                                        title: if removed { "Restore the base's member" } else { "Drop the base's member" },
                                        onclick: move |_| draft::remove_member(&mut editor.write().profile().chord[index], &source_drop, !removed),
                                        if removed { "↺" } else { "−" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Pick { options: free, value: String::new(),
                onchange: move |source: String| {
                    if !source.is_empty() {
                        editor.write().profile().chord[index].buttons.insert(source, BindingToml::Target("none".to_string()));
                    }
                }
            }
        }
    }
}
