//! The shipped timers (`docs/architecture.md` sections 10.3 and 10.4): the launchd agent and
//! the systemd units parse, each runs `cardinal reconcile --trigger timer` and nothing else,
//! and their interval is the one the disk prediction assumes.

use std::collections::BTreeMap;
use std::path::Path;

fn example(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// A property list value, as far as the agent uses them.
#[derive(Debug, PartialEq)]
enum Value {
    String(String),
    Integer(i64),
    Bool(bool),
    Array(Vec<Value>),
    Dict(BTreeMap<String, Value>),
}

/// The XML property list `text`: its top `<dict>`. Panics on anything else.
fn plist(text: &str) -> BTreeMap<String, Value> {
    let mut rest = text.to_owned();
    while let Some(start) = rest.find("<!--") {
        let end = rest[start..].find("-->").unwrap() + start + 3;
        rest.replace_range(start..end, "");
    }
    let body = rest
        .split_once("<plist version=\"1.0\">")
        .unwrap()
        .1
        .rsplit_once("</plist>")
        .unwrap()
        .0;
    let mut tokens = tags(body).into_iter().peekable();
    let Value::Dict(top) = value(&mut tokens) else {
        panic!("the top value is not a dict");
    };
    assert!(tokens.next().is_none(), "one top value");
    top
}

/// The tags and texts of `xml`, whitespace between tags dropped.
fn tags(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        let text = rest[..open].trim();
        if !text.is_empty() {
            out.push(text.to_owned());
        }
        let close = rest[open..].find('>').unwrap() + open;
        out.push(rest[open..=close].to_owned());
        rest = &rest[close + 1..];
    }
    assert!(rest.trim().is_empty(), "trailing text: {rest}");
    out
}

fn value(tokens: &mut std::iter::Peekable<std::vec::IntoIter<String>>) -> Value {
    let tag = tokens.next().unwrap();
    let mut text = |end: &str| {
        let text = tokens.next().unwrap();
        assert_eq!(tokens.next().as_deref(), Some(end));
        text
    };
    match tag.as_str() {
        "<string>" => Value::String(text("</string>")),
        "<integer>" => Value::Integer(text("</integer>").parse().unwrap()),
        "<true/>" => Value::Bool(true),
        "<false/>" => Value::Bool(false),
        "<array>" => {
            let mut items = Vec::new();
            while tokens.peek().map(String::as_str) != Some("</array>") {
                items.push(value(tokens));
            }
            tokens.next();
            Value::Array(items)
        }
        "<dict>" => {
            let mut dict = BTreeMap::new();
            while tokens.peek().map(String::as_str) != Some("</dict>") {
                assert_eq!(tokens.next().as_deref(), Some("<key>"));
                let key = tokens.next().unwrap();
                assert_eq!(tokens.next().as_deref(), Some("</key>"));
                let value = value(tokens);
                assert!(dict.insert(key.clone(), value).is_none(), "{key} twice");
            }
            tokens.next();
            Value::Dict(dict)
        }
        other => panic!("unexpected {other}"),
    }
}

/// The sections of a systemd unit file, each a list of `(key, value)`.
fn unit(text: &str) -> BTreeMap<String, Vec<(String, String)>> {
    let mut sections: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut current = None;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = Some(name.to_owned());
            sections.entry(name.to_owned()).or_default();
            continue;
        }
        let (key, value) = line.split_once('=').unwrap_or_else(|| panic!("{line}"));
        let section = current.clone().expect("a key before any section");
        sections
            .get_mut(&section)
            .unwrap()
            .push((key.to_owned(), value.to_owned()));
    }
    sections
}

fn get<'a>(section: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    section
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .collect()
}

const INTERVAL_S: u64 = cardinal::reconcile::TIMER_INTERVAL_MS / 1000;

#[test]
fn the_launchd_agent_runs_only_the_timer_pass() {
    let agent = plist(&example("launchd/dev.cardinal.reconcile.plist"));
    let keys: Vec<&str> = agent.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "EnvironmentVariables",
            "Label",
            "LowPriorityIO",
            "ProcessType",
            "ProgramArguments",
            "RunAtLoad",
            "StandardErrorPath",
            "StandardOutPath",
            "StartInterval",
        ]
    );
    let Value::Array(argv) = &agent["ProgramArguments"] else {
        panic!("ProgramArguments is not an array");
    };
    let argv: Vec<&str> = argv
        .iter()
        .map(|v| match v {
            Value::String(s) => s.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(
        argv[0].starts_with('/') && argv[0].ends_with("/cardinal"),
        "{argv:?}"
    );
    assert_eq!(argv[1..], ["reconcile", "--trigger", "timer"]);
    assert_eq!(
        agent["StartInterval"],
        Value::Integer(i64::try_from(INTERVAL_S).unwrap())
    );
    assert_eq!(agent["RunAtLoad"], Value::Bool(true));
    assert_eq!(agent["LowPriorityIO"], Value::Bool(true));
    assert_eq!(agent["ProcessType"], Value::String("Background".into()));
    for out in ["StandardOutPath", "StandardErrorPath"] {
        assert_eq!(agent[out], Value::String("/dev/null".into()));
    }
    let Value::Dict(env) = &agent["EnvironmentVariables"] else {
        panic!("EnvironmentVariables is not a dict");
    };
    assert_eq!(env.keys().collect::<Vec<_>>(), ["PATH"]);
}

#[test]
fn the_systemd_units_run_only_the_timer_pass() {
    let service = unit(&example("systemd/cardinal-reconcile.service"));
    let timer = unit(&example("systemd/cardinal-reconcile.timer"));
    let run = &service["Service"];
    assert_eq!(get(run, "Type"), ["oneshot"]);
    // The detached arbiter launcher outlives the pass (section 10.5).
    assert_eq!(get(run, "KillMode"), ["process"]);
    let exec: Vec<&(String, String)> = service
        .values()
        .flatten()
        .filter(|(k, _)| k.starts_with("Exec"))
        .collect();
    assert_eq!(exec.len(), 1, "{exec:?}");
    assert_eq!(exec[0].0, "ExecStart");
    let argv: Vec<&str> = exec[0].1.split_whitespace().collect();
    assert!(
        (argv[0].starts_with('/') || argv[0].starts_with("%h/")) && argv[0].ends_with("/cardinal"),
        "{argv:?}"
    );
    assert_eq!(argv[1..], ["reconcile", "--trigger", "timer"]);
    assert!(
        get(run, "Environment")
            .iter()
            .any(|e| e.starts_with("PATH=")),
        "{run:?}"
    );

    let fires = &timer["Timer"];
    assert_eq!(
        get(fires, "OnUnitActiveSec"),
        [format!("{}min", INTERVAL_S / 60)]
    );
    assert_eq!(get(fires, "OnActiveSec").len(), 1);
    // The timer starts the service of its own name.
    assert_eq!(get(fires, "Unit"), Vec::<&str>::new());
    assert_eq!(get(&timer["Install"], "WantedBy"), ["timers.target"]);
}

/// `plutil` accepts the agent (macOS).
#[cfg(target_os = "macos")]
#[test]
fn plutil_accepts_the_agent() {
    let scope = cardinal_test_support::Scope::new();
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/launchd/dev.cardinal.reconcile.plist");
    let out = scope
        .command("plutil", scope.root())
        .arg("-lint")
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}
