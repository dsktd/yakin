//! The readable name of an app: the `Name=` of its desktop entry, else the app id.

use std::env;
use std::path::PathBuf;

/// The name shown for `app_id`; `none` when no app is selected.
pub fn display(app_id: &str) -> String {
    if app_id.is_empty() {
        return "none".to_string();
    }
    data_dirs()
        .into_iter()
        .find_map(|dir| {
            let text =
                std::fs::read_to_string(dir.join("applications").join(format!("{app_id}.desktop")))
                    .ok()?;
            entry_name(&text)
        })
        .unwrap_or_else(|| app_id.to_string())
}

/// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`, with the spec's defaults.
fn data_dirs() -> Vec<PathBuf> {
    let home = env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter().chain(env::split_paths(&dirs)).collect()
}

/// The unlocalized `Name=` of the `[Desktop Entry]` group.
fn entry_name(text: &str) -> Option<String> {
    let mut in_entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry
            && let Some(name) = line.strip_prefix("Name=")
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_entry_name() {
        let text = "[Desktop Action x]\nName=Wrong\n[Desktop Entry]\nName[de]=Foo\nName=Firefox\n";
        assert_eq!(entry_name(text).as_deref(), Some("Firefox"));
        assert_eq!(entry_name("[Desktop Entry]\nExec=x"), None);
    }

    #[test]
    fn falls_back_to_the_id() {
        assert_eq!(display(""), "none");
        assert_eq!(display("no.such.app-zzz"), "no.such.app-zzz");
    }
}
