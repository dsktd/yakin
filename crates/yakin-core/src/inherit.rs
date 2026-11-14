//! Profiles laid over others: a profile naming a `base` holds only what it changes, and the
//! effective profile is its base's with those changes [`merge`]d on top, like a drop-in.
//! Removing a change falls back to the base. Chords key by their `hold`: one on a hold the
//! base has merges into the base's chord member by member, one on a new hold is added, and
//! `remove = true` or `remove = ["a"]` drops the base's chord or those members of it.

use std::collections::BTreeMap;

use anyhow::anyhow;
use serde::Deserialize;
use toml::Table;
use toml::Value;

use crate::Result;
use crate::layer::merge;
use crate::profile::ProfileToml;

/// The profile `name` with its bases laid under it, nearest last; its `base` is left out.
pub fn resolve(
    profiles: &BTreeMap<String, ProfileToml>,
    name: &str,
) -> Result<ProfileToml> {
    let chain = chain(profiles, name)?;
    let mut all = Table::new();
    let mut chords = Vec::new();
    for profile in chain.iter().rev() {
        let Value::Table(mut table) = Value::try_from(profile)? else {
            unreachable!("a profile serializes as a table");
        };
        if let Some(Value::Array(over)) = table.remove("chord") {
            merge_chords(&mut chords, over);
        }
        merge(&mut all, table);
    }
    all.remove("base");
    if !chords.is_empty() {
        all.insert("chord".into(), Value::Array(chords));
    }
    Ok(ProfileToml::deserialize(all)?)
}

/// Lay one profile's chords onto those of its bases. A chord matches only a base's, so a
/// profile's own chords keep their order and repeats, as without a base.
fn merge_chords(
    chords: &mut Vec<Value>,
    over: Vec<Value>,
) {
    let inherited = chords.len();
    for chord in over {
        let Value::Table(mut chord) = chord else {
            unreachable!("a chord serializes as a table");
        };
        let remove = chord.remove("remove");
        let hold = chord.get("hold").cloned();
        let base = chords[..inherited]
            .iter()
            .position(|base| base.get("hold") == hold.as_ref());
        match (remove, base) {
            (Some(Value::Boolean(true)), Some(base)) => {
                // Left in place so the later matches keep their positions
                chords[base] = Value::Boolean(false);
            }
            (Some(Value::Boolean(true)), None) => {}
            (remove, Some(base)) => {
                let Value::Table(base) = &mut chords[base] else {
                    continue;
                };
                if let Some(Value::Array(members)) = remove {
                    for member in members.iter().filter_map(Value::as_str) {
                        base.remove(member);
                    }
                }
                merge(base, chord);
            }
            (_, None) => chords.push(Value::Table(chord)),
        }
    }
    chords.retain(Value::is_table);
}

/// The profile and its bases, itself first.
fn chain<'a>(
    profiles: &'a BTreeMap<String, ProfileToml>,
    name: &str,
) -> Result<Vec<&'a ProfileToml>> {
    let mut names = vec![name];
    let mut chain = vec![
        profiles
            .get(name)
            .ok_or_else(|| anyhow!("No profile named {name}"))?,
    ];
    while let Some(base) = chain.last().unwrap().base.as_deref() {
        if names.contains(&base) {
            return Err(anyhow!("base: {} -> {base} loops", names.join(" -> ")));
        }
        let profile = profiles
            .get(base)
            .ok_or_else(|| anyhow!("base: No profile named {base}"))?;
        names.push(base);
        chain.push(profile);
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::BindingToml;
    use crate::profile::Config;
    use crate::profile::ConfigToml;

    const SWITCH: &str = r#"
        [profile.switch]
        deadzone.stick = 0.2
        buttons.a = "pad:east"
        buttons.grip_l2 = "none"
        [profile.nx2navi]
        base = "switch"
        buttons.grip_l2 = "pad:dpad_down"
        deadzone.trigger = 0.1
    "#;

    fn profiles(text: &str) -> BTreeMap<String, ProfileToml> {
        ConfigToml::parse(text).unwrap().profile
    }

    fn button(
        profile: &ProfileToml,
        name: &str,
    ) -> Option<BindingToml> {
        profile.buttons.get(name).cloned()
    }

    #[test]
    fn test_overrides_lay_over_the_base() {
        let resolved = resolve(&profiles(SWITCH), "nx2navi").unwrap();
        assert_eq!(resolved.base, None);
        assert_eq!(
            button(&resolved, "grip_l2"),
            Some(BindingToml::Target("pad:dpad_down".into()))
        );
        assert_eq!(
            button(&resolved, "a"),
            Some(BindingToml::Target("pad:east".into()))
        );
        assert_eq!(resolved.deadzone.stick, Some(0.2));
        assert_eq!(resolved.deadzone.trigger, Some(0.1));
    }

    #[test]
    fn test_removed_override_falls_back() {
        let mut all = profiles(SWITCH);
        all.get_mut("nx2navi").unwrap().buttons.remove("grip_l2");
        let resolved = resolve(&all, "nx2navi").unwrap();
        assert_eq!(
            button(&resolved, "grip_l2"),
            Some(BindingToml::Target("none".into()))
        );
    }

    #[test]
    fn test_bases_chain() {
        let text =
            format!("{SWITCH}[profile.speedrun]\nbase = \"nx2navi\"\nbuttons.a = \"pad:south\"\n");
        let resolved = resolve(&profiles(&text), "speedrun").unwrap();
        assert_eq!(
            button(&resolved, "grip_l2"),
            Some(BindingToml::Target("pad:dpad_down".into()))
        );
        assert_eq!(
            button(&resolved, "a"),
            Some(BindingToml::Target("pad:south".into()))
        );
    }

    #[test]
    fn test_bad_bases() {
        let err = Config::parse("[profile.a]\nbase = \"nope\"").unwrap_err();
        assert_eq!(err.to_string(), "Profile a: base: No profile named nope");
        let err =
            Config::parse("[profile.a]\nbase = \"b\"\n[profile.b]\nbase = \"a\"").unwrap_err();
        assert_eq!(err.to_string(), "Profile a: base: a -> b -> a loops");
    }

    #[test]
    fn test_config_resolves() {
        let config = Config::parse(SWITCH).unwrap();
        let switch = config.get("switch").unwrap();
        let nx2navi = config.get("nx2navi").unwrap();
        assert_eq!(nx2navi.deadzone_stick, switch.deadzone_stick);
        assert_ne!(nx2navi.buttons, switch.buttons);
    }

    const CHORDS: &str = r#"
        [[profile.switch.chord]]
        hold = "steam"
        a = "key:KEY_A"
        b = { press = "key:KEY_B", turbo_ms = 100 }
        [[profile.switch.chord]]
        hold = "view"
        x = "key:KEY_X"
        [profile.quick]
        base = "switch"
        [[profile.quick.chord]]
        hold = "quick"
        y = "key:KEY_Y"
        [profile.nx2navi]
        base = "quick"
        buttons.grip_l2 = "pad:dpad_down"
    "#;

    fn chords(
        text: &str,
        name: &str,
    ) -> Vec<String> {
        let resolved = resolve(&profiles(text), name).unwrap();
        resolved
            .chord
            .iter()
            .map(|chord| {
                let members: Vec<String> = chord
                    .buttons
                    .iter()
                    .map(|(source, binding)| format!("{source}={}", binding.press().unwrap_or("-")))
                    .collect();
                assert_eq!(chord.remove, None);
                format!("{}: {}", chord.hold, members.join(" "))
            })
            .collect()
    }

    #[test]
    fn test_chords_add_by_hold_down_a_chain() {
        assert_eq!(
            chords(CHORDS, "nx2navi"),
            [
                "steam: a=key:KEY_A b=key:KEY_B",
                "view: x=key:KEY_X",
                "quick: y=key:KEY_Y"
            ]
        );
    }

    #[test]
    fn test_chord_on_a_base_hold_merges() {
        let text = format!(
            "{CHORDS}[[profile.nx2navi.chord]]\nhold = \"steam\"\na = \"key:KEY_C\"\nb.turbo_ms = 50\n"
        );
        assert_eq!(
            chords(&text, "nx2navi")[0],
            "steam: a=key:KEY_C b=key:KEY_B"
        );
        let resolved = resolve(&profiles(&text), "nx2navi").unwrap();
        let BindingToml::Activators(b) = &resolved.chord[0].buttons["b"] else {
            panic!("b is a table");
        };
        assert_eq!(b.turbo_ms, Some(50));
    }

    #[test]
    fn test_chord_removal() {
        let text = format!(
            "{CHORDS}[[profile.nx2navi.chord]]\nhold = \"view\"\nremove = true\n\
             [[profile.nx2navi.chord]]\nhold = \"steam\"\nremove = [\"a\"]\n"
        );
        assert_eq!(
            chords(&text, "nx2navi"),
            ["steam: b=key:KEY_B", "quick: y=key:KEY_Y"]
        );
        // The base itself keeps them
        assert_eq!(chords(&text, "switch").len(), 2);
    }

    #[test]
    fn test_chords_without_base_load_as_written() {
        let text = "[[profile.p.chord]]\nhold = \"view\"\na = \"key:KEY_A\"\n\
                    [[profile.p.chord]]\nhold = \"view\"\nb = \"key:KEY_B\"\n";
        assert_eq!(
            chords(text, "p"),
            ["view: a=key:KEY_A", "view: b=key:KEY_B"]
        );
    }
}
