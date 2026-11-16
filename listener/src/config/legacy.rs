//! Reading profiles saved by an older build: several Display Views per
//! Channel, and TCP Listener Channels.
//!
//! A Channel has one view (§48), saved as `display.view`. Earlier profiles
//! saved a list, `display.views`, and a `PauseDisplay` action could name one of
//! them by index with `view = N`. [`upgrade`] rewrites that shape into the
//! current one on the parsed TOML, before the strict typed parse, so a profile
//! saved by an older build loads and is written back in the new form.

use toml::{Table, Value};

/// What [`upgrade`] did to a profile's TOML.
pub(super) struct Upgrade {
    /// Whether anything was rewritten. When it is not, a parse failure belongs
    /// to the file as written and is reported as it stands, with its line.
    pub changed: bool,
    /// Each thing the old shape said that the current one cannot, worded for
    /// the person who loaded the profile.
    pub warnings: Vec<String>,
}

/// Rewrite the old `views` list and `PauseDisplay` `view` index in place.
pub(super) fn upgrade(doc: &mut Table) -> Upgrade {
    let mut out = Upgrade {
        changed: false,
        warnings: Vec::new(),
    };
    if let Some(display) = doc
        .get_mut("defaults")
        .and_then(Value::as_table_mut)
        .and_then(|defaults| defaults.get_mut("display"))
        .and_then(Value::as_table_mut)
    {
        upgrade_display(display, "the profile's defaults", &mut out);
    }
    let Some(channels) = doc.get_mut("channels").and_then(Value::as_array_mut) else {
        return out;
    };
    for channel in channels.iter_mut().filter_map(Value::as_table_mut) {
        let name = channel
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("(unnamed)")
            .to_owned();
        let who = format!("channel \"{name}\"");
        if let Some(display) = channel.get_mut("display").and_then(Value::as_table_mut) {
            upgrade_display(display, &who, &mut out);
        }
        let Some(rules) = channel.get_mut("match_rules").and_then(Value::as_array_mut) else {
            continue;
        };
        for rule in rules.iter_mut().filter_map(Value::as_table_mut) {
            let rule_name = rule
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)")
                .to_owned();
            let Some(actions) = rule.get_mut("actions").and_then(Value::as_array_mut) else {
                continue;
            };
            actions.retain_mut(|action| {
                upgrade_pause_action(action, &format!("{who}, rule \"{rule_name}\""), &mut out)
            });
        }
    }
    out
}

/// Replace `views = [..]` with `view = <the first>`. An empty list becomes no
/// `view` at all, which takes the default view.
fn upgrade_display(display: &mut Table, who: &str, out: &mut Upgrade) {
    let Some(views) = display.remove("views") else {
        return;
    };
    let Value::Array(mut views) = views else {
        // Not a list: put it back for the typed parse to name.
        display.insert("views".to_owned(), views);
        return;
    };
    out.changed = true;
    let dropped = views.len().saturating_sub(1);
    if dropped > 0 {
        out.warnings.push(format!(
            "{who} had {} display views; a channel has one now, so the first was kept \
             and the other {dropped} dropped",
            views.len()
        ));
    }
    if !views.is_empty() && !display.contains_key("view") {
        display.insert("view".to_owned(), views.swap_remove(0));
    }
}

/// Drop the `view` index from a `PauseDisplay` action. Returns whether the
/// action stays: one that named a view past the first pointed at a view that no
/// longer exists, and that view never froze the display the Channel shows.
fn upgrade_pause_action(action: &mut Value, who: &str, out: &mut Upgrade) -> bool {
    let Some(table) = action.as_table_mut() else {
        return true;
    };
    if table.get("kind").and_then(Value::as_str) != Some("PauseDisplay") {
        return true;
    }
    let Some(view) = table.remove("view") else {
        return true;
    };
    out.changed = true;
    match view.as_integer() {
        Some(0) => true,
        Some(index) => {
            out.warnings.push(format!(
                "{who}: a PauseDisplay action named display view {index}, which no longer \
                 exists, so the action was removed"
            ));
            false
        }
        // Not an integer: put it back for the typed parse to refuse.
        None => {
            table.insert("view".to_owned(), view);
            true
        }
    }
}

/// Remove each Channel that names a TCP Listener, by its `kind` or its
/// interface `type`, and return one warning per Channel removed.
///
/// A TCP Listener Channel is a shape this build no longer has, so the strict
/// typed parse would refuse the whole profile for it. Removing it first lets
/// the rest load; the warning names it, so it is not lost silently.
pub(super) fn drop_tcp_channels(doc: &mut Table) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(channels) = doc.get_mut("channels").and_then(Value::as_array_mut) else {
        return warnings;
    };
    channels.retain(|channel| {
        let Some(table) = channel.as_table() else {
            return true;
        };
        let kind = table.get("kind").and_then(Value::as_str);
        let interface = table
            .get("interface")
            .and_then(Value::as_table)
            .and_then(|interface| interface.get("type"))
            .and_then(Value::as_str);
        let is_tcp = matches!(kind, Some("TcpListener" | "TcpConnection"))
            || interface == Some("TcpListener");
        if is_tcp {
            let name = table
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)");
            warnings.push(format!(
                "channel \"{name}\" is a TCP Listener, which this release does not offer, \
                 so it was left out"
            ));
        }
        !is_tcp
    });
    warnings
}
