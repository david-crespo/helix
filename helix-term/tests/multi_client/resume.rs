//! Coming back in the same pane after the terminal restarts (OSC 7577). A terminal that supports
//! it sets `OSC7577_TOKEN` and, after a restart, runs the command `hx` reported, which is
//! `hx --resume <state file>`. These tests stand in for that terminal.

use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use crate::{
    harness::{Client, Env},
    wait_until,
};

const TOKEN: &str = "test-token-0123456789abcdef";

/// The terminal's token, and a short idle time before state files are written.
const VARS: [(&str, &str); 2] = [("OSC7577_TOKEN", TOKEN), ("HELIX_RESUME_IDLE_MS", "100")];

fn client(env: &Env, args: &[&str]) -> Client {
    env.client_with(&env.project(), &VARS, args)
}

fn state_dir(env: &Env) -> PathBuf {
    env.root().join("state/helix/resume")
}

/// The state directory, made as `hx` would make it.
fn private_state_dir(env: &Env) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = state_dir(env);
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn state_files(env: &Env) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(state_dir(env)) else {
        return Vec::new();
    };
    entries.map(|entry| entry.unwrap().path()).collect()
}

fn read_state(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Waits for a state file to have a layout that `done` accepts, and returns its path.
fn wait_for_layout(env: &Env, done: impl Fn(&Value) -> bool) -> PathBuf {
    let found = || {
        state_files(env)
            .into_iter()
            .find(|path| read_state(path).is_some_and(|state| done(&state["layout"])))
    };
    wait_until("the state file to have the layout", || found().is_some());
    found().unwrap()
}

/// The paths of the views in a layout, in order.
fn views(node: &Value) -> Vec<String> {
    if let Some(view) = node.get("view") {
        let path = view["path"].as_str().unwrap_or("[scratch]");
        return vec![Path::new(path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into()];
    }
    let children = node["split"]["children"].as_array().unwrap();
    children.iter().flat_map(views).collect()
}

/// Resumes the instance whose state file is `path`, as the terminal would after restarting, and
/// checks that it comes back exactly as it was: the editor's first save of it writes the same
/// file again.
fn resume_unchanged(env: &Env, path: &Path) -> Client {
    let before = fs::read_to_string(path).unwrap();
    // Set each view's primary selection index out of range so restoring clamps it back to 0.
    // That distinguishes the editor's write from the client's initial pass-through write.
    let mut state = read_state(path).unwrap();
    set_primaries(&mut state["layout"]["root"], 99);
    fs::write(path, serde_json::to_string(&state).unwrap()).unwrap();
    let resumed = client(env, &["--resume", path.to_str().unwrap()]);
    wait_until("the resumed editor to save the same state", || {
        fs::read_to_string(path).is_ok_and(|after| after == before)
    });
    resumed
}

fn set_primaries(node: &mut Value, primary: usize) {
    if let Some(view) = node.get_mut("view") {
        view["primary"] = json!(primary);
    } else {
        for child in node["split"]["children"].as_array_mut().unwrap() {
            set_primaries(child, primary);
        }
    }
}

/// The bodies of the OSC 7577 reports the terminal received, each checked against its MAC.
fn reports(client: &Client) -> Vec<String> {
    let output = String::from_utf8_lossy(&client.output()).into_owned();
    let sequences = output.split("\x1b]7577;").skip(1);
    sequences
        .map(|rest| {
            let (sequence, _) = rest.split_once("\x1b\\").expect("a whole sequence");
            let (body, mac) = sequence.rsplit_once(":mac=").expect("a MAC");
            let expected = Sha256::digest(format!("{body}:{TOKEN}"));
            let expected: String = expected.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(mac, expected, "the MAC of {body:?}");
            body.to_owned()
        })
        .collect()
}

/// Whether `body` is a report from `pid`, at some time, that ends in `rest`.
fn is_report(body: &str, pid: u32, rest: &str) -> bool {
    let Some(body) = body.strip_prefix(&format!("pid={pid}:time=")) else {
        return false;
    };
    body.split_once(':')
        .is_some_and(|(time, body)| time.parse::<u64>().is_ok() && body == rest)
}

fn reported(client: &Client, path: &Path) -> bool {
    let argv = format!(
        "argv={},{},{}",
        base64(b"hx"),
        base64(b"--resume"),
        base64(path.to_str().unwrap().as_bytes())
    );
    let pid = client.pid();
    reports(client)
        .iter()
        .any(|body| is_report(body, pid, &argv))
}

fn cleared(client: &Client) -> bool {
    let pid = client.pid();
    reports(client)
        .iter()
        .any(|body| is_report(body, pid, "clear=1"))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk.iter().enumerate())
            .fold(0u32, |n, (i, &byte)| n | u32::from(byte) << (16 - 8 * i));
        for i in 0..4 {
            out.push(match i <= chunk.len() {
                true => ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char,
                false => '=',
            });
        }
    }
    out
}

fn split_env() -> Env {
    let env = Env::new();
    for name in ["a", "b", "c", "d"] {
        env.write(&format!("{name}.txt"), "one\ntwo\nthree\nfour\n");
    }
    env
}

#[test]
fn session_client_comes_back_with_its_layout() {
    let env = split_env();
    let mut a = client(&env, &["a.txt", "d.txt"]);
    a.command("vsplit b.txt");
    a.command("hsplit c.txt");
    a.keys("jjl");
    let path = wait_for_layout(&env, |layout| {
        views(&layout["root"]) == ["a.txt", "b.txt", "c.txt"]
            && layout["root"]["split"]["children"][1]["split"]["layout"] == "Horizontal"
            && layout["focus"] == 2
            && layout["root"]["split"]["children"][1]["split"]["children"][1]["view"]["selection"]
                == json!([[{"row": 2, "col": 1}, {"row": 2, "col": 2}]])
    });
    let state = read_state(&path).unwrap();
    assert!(state["session"].is_string(), "{state}");
    let buffers = state["layout"]["buffers"].as_array().unwrap();
    assert!(buffers.len() == 1 && buffers[0].as_str().unwrap().ends_with("d.txt"));
    assert!(reported(&a, &path), "the client reports how to resume");

    // The terminal goes away, as when it restarts. The file stays for the new one.
    a.hang_up();
    a.wait_exit();
    assert!(path.exists());

    let resumed = resume_unchanged(&env, &path);
    for name in ["a.txt", "b.txt", "c.txt"] {
        resumed.wait_for(name);
    }
    resumed.wait_for("3:2");
    assert!(reported(&resumed, &path), "a resumed client reports again");
}

#[test]
fn nested_splits_come_back_as_they_were() {
    let env = split_env();
    let a = client(&env, &["a.txt"]);
    a.command("vsplit b.txt");
    a.command("hsplit c.txt");
    a.command("vsplit d.txt");
    // Closing b leaves its sibling, a vertical split, in the vertical root.
    a.keys("<C-w>k");
    a.command("q");
    let path = wait_for_layout(&env, |layout| {
        let root = &layout["root"]["split"];
        views(&layout["root"]) == ["a.txt", "c.txt", "d.txt"]
            && root["layout"] == "Vertical"
            && root["children"][1]["split"]["layout"] == "Vertical"
    });
    drop(a);
    let resumed = resume_unchanged(&env, &path);
    resumed.wait_for("d.txt");
}

#[test]
fn turned_root_comes_back_turned() {
    let env = split_env();
    let a = client(&env, &["--no-session", "a.txt"]);
    a.command("vsplit b.txt");
    a.keys("<C-w>t");
    let path = wait_for_layout(&env, |layout| {
        layout["root"]["split"]["layout"] == "Horizontal" && views(&layout["root"]).len() == 2
    });
    drop(a);
    let resumed = resume_unchanged(&env, &path);
    resumed.wait_for("b.txt");
}

#[test]
fn standalone_comes_back_after_a_hang_up() {
    let env = split_env();
    // Long enough that only the hang-up can have saved the split.
    let vars = [("OSC7577_TOKEN", TOKEN), ("HELIX_RESUME_IDLE_MS", "60000")];
    let mut a = env.client_with(&env.project(), &vars, &["--no-session", "a.txt"]);
    let [path] = state_files(&env).try_into().unwrap();
    assert!(reported(&a, &path), "{}", a.screen());
    a.command("vsplit b.txt");
    a.wait_for("b.txt");
    a.hang_up();
    a.wait_exit();

    let state = read_state(&path).unwrap();
    assert_eq!(views(&state["layout"]["root"]), ["a.txt", "b.txt"]);
    assert!(state["session"].is_null(), "{state}");
    let mut resumed = resume_unchanged(&env, &path);
    resumed.wait_for("b.txt");

    resumed.command("qa");
    resumed.wait_exit();
    assert!(!path.exists(), "quitting deletes the state file");
    wait_until("the clear report", || cleared(&resumed));
}

#[test]
fn session_client_saves_its_layout_when_its_terminal_goes() {
    let env = split_env();
    // Long enough that only losing the terminal can have saved the split.
    let vars = [("OSC7577_TOKEN", TOKEN), ("HELIX_RESUME_IDLE_MS", "60000")];
    let mut a = env.client_with(&env.project(), &vars, &["a.txt"]);
    a.command("vsplit b.txt");
    a.wait_for("b.txt");
    a.hang_up();
    a.wait_exit();
    let path = wait_for_layout(&env, |layout| views(&layout["root"]) == ["a.txt", "b.txt"]);
    let resumed = resume_unchanged(&env, &path);
    resumed.wait_for("b.txt");
}

/// The usual way a session's client loses its terminal when the terminal restarts: the terminal
/// hangs up, while other clients keep the session going.
#[test]
fn losing_one_of_several_terminals_keeps_its_record() {
    let env = split_env();
    let _b = client(&env, &["b.txt"]);
    // Ignoring SIGHUP keeps the client and its connection to the server going after its terminal
    // hangs up, as they are until the signal reaches it.
    let mut a = env.shell(&format!(
        "trap '' HUP; OSC7577_TOKEN={TOKEN} HELIX_RESUME_IDLE_MS=100 exec \"$HX\" a.txt"
    ));
    a.wait_for("NOR");
    a.command("vsplit c.txt");
    let path = wait_for_layout(&env, |layout| views(&layout["root"]) == ["a.txt", "c.txt"]);
    assert!(reported(&a, &path));
    a.close_terminal();
    a.wait_exit();
    assert!(!cleared(&a));
    let state = read_state(&path).expect("the state file is still there");
    assert_eq!(views(&state["layout"]["root"]), ["a.txt", "c.txt"]);
}

/// A standalone `hx`'s terminal can go away before SIGHUP reaches it, or without SIGHUP at all.
#[test]
fn standalone_saves_when_its_terminal_closes() {
    let env = split_env();
    let mut a = env.shell(&format!(
        "trap '' HUP; OSC7577_TOKEN={TOKEN} HELIX_RESUME_IDLE_MS=60000 \
         exec \"$HX\" --no-session a.txt"
    ));
    a.wait_for("NOR");
    a.command("vsplit b.txt");
    a.wait_for("b.txt");
    a.close_terminal();
    a.wait_exit();
    assert!(!cleared(&a));
    let [path] = state_files(&env).try_into().unwrap();
    let state = read_state(&path).unwrap();
    assert_eq!(views(&state["layout"]["root"]), ["a.txt", "b.txt"]);
}

/// A client leaving doesn't delay writing another client's changes: the pending write keeps its
/// deadline.
#[test]
fn losing_a_client_keeps_the_others_saves_due() {
    let env = split_env();
    let vars = [("OSC7577_TOKEN", TOKEN), ("HELIX_RESUME_IDLE_MS", "3000")];
    let a = env.client_with(&env.project(), &vars, &["a.txt"]);
    let b = env.client_with(&env.project(), &vars, &["b.txt"]);
    a.command("vsplit c.txt");
    a.wait_for("c.txt");
    let changed = Instant::now();
    thread::sleep(Duration::from_secs(2));
    drop(b);
    wait_for_layout(&env, |layout| views(&layout["root"]) == ["a.txt", "c.txt"]);
    // Due 3 s after the change. Put off, it would be 3 s after `b` went, 5 s after the change.
    assert!(
        changed.elapsed() < Duration::from_secs(4),
        "{:?}",
        changed.elapsed()
    );
}

#[test]
fn state_file_waits_for_a_pause_in_input() {
    let env = split_env();
    let vars = [("OSC7577_TOKEN", TOKEN), ("HELIX_RESUME_IDLE_MS", "1000")];
    let a = env.client_with(&env.project(), &vars, &["a.txt"]);
    let path = wait_for_layout(&env, |layout| views(&layout["root"]) == ["a.txt"]);
    let before = fs::read_to_string(&path).unwrap();
    for key in "jjkkjjkkj".chars() {
        a.keys(&key.to_string());
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            before,
            "written while typing"
        );
    }
    a.wait_for("2:1");
    wait_for_layout(&env, |layout| {
        layout["root"]["split"]["children"][0]["view"]["selection"][0][1]["row"] == 1
    });
}

#[test]
fn continuing_after_a_suspend_reports_again() {
    for args in ["", "--no-session "] {
        let env = split_env();
        // Job control needs a shell: a session leader's process group ignores SIGTSTP.
        let shell = env.interactive_shell();
        shell.keys(&format!("OSC7577_TOKEN={TOKEN} \"$HX\" {args}a.txt<ret>"));
        shell.wait_for("NOR");
        let count = || reports(&shell).len();
        assert_eq!(count(), 1, "{args}");
        shell.keys("<C-z>");
        shell.wait_for("Stopped");
        assert_eq!(count(), 1, "{args}");
        shell.keys("fg<ret>");
        wait_until("another report", || count() == 2);
    }
}

#[test]
fn quitting_clears_the_report_and_deletes_the_file() {
    let env = split_env();
    let mut a = client(&env, &["a.txt"]);
    let path = wait_for_layout(&env, |layout| views(&layout["root"]) == ["a.txt"]);
    a.command("q");
    a.wait_exit();
    assert!(!path.exists());
    wait_until("the clear report", || cleared(&a));
    assert!(
        !a.received(TOKEN.as_bytes()),
        "the token itself is never sent"
    );
}

#[test]
fn nothing_is_reported_without_a_token() {
    let env = split_env();
    let mut a = env.client(&["a.txt"]);
    let mut b = env.client(&["--no-session", "a.txt"]);
    a.never_shows("never");
    assert!(state_files(&env).is_empty());
    assert!(!a.received(b"\x1b]7577;") && !b.received(b"\x1b]7577;"));
    a.command("q");
    b.command("q");
    a.wait_exit();
    b.wait_exit();
    assert!(!a.received(b"\x1b]7577;") && !b.received(b"\x1b]7577;"));
}

/// Nothing the editor starts can report for it: the token isn't in their environment, not
/// even the session server's.
#[test]
fn token_stays_out_of_what_hx_starts() {
    let env = split_env();
    let a = client(&env, &["a.txt"]);
    a.command("sh echo token=${OSC7577_TOKEN:-none}");
    a.wait_for("token=none");
    let b = client(&env, &["--no-session", "a.txt"]);
    b.command("sh echo token=${OSC7577_TOKEN:-none}");
    b.wait_for("token=none");
}

#[test]
fn resume_refuses_files_it_didnt_write() {
    let env = split_env();
    let mut shell = env.shell("\"$HX\" --resume a.txt; echo exit=$?");
    shell.wait_for("exit=1");
    shell.wait_exit();
    assert_eq!(env.read("a.txt"), "one\ntwo\nthree\nfour\n");
}

/// A state file edited by hand, or saved before its files shrank, can have positions, offsets,
/// and indexes out of range. Restoring clamps them.
#[test]
fn state_file_with_numbers_out_of_range_still_restores() {
    for session in [json!(null), json!("hostile")] {
        let env = split_env();
        let max = usize::MAX;
        let far = json!({"row": max, "col": max});
        let view = |path: &str, selection: Value| {
            json!({"view": {
                "path": env.path(path),
                "selection": selection,
                "primary": max,
                "top": far,
                "horizontal_offset": max,
                "vertical_offset": max,
            }})
        };
        let state = json!({
            "cwd": env.project(),
            "session": session,
            "config": null,
            "layout": {
                "root": {"split": {"layout": "Horizontal", "children": [
                    view("a.txt", json!([[far, {"row": 0, "col": max}]])),
                    {"split": {"layout": "Vertical", "children": []}},
                    view("b.txt", json!([])),
                    {"view": {
                        "path": null, "selection": [[far, far]], "primary": 0, "top": far,
                        "horizontal_offset": 0, "vertical_offset": max,
                    }},
                ]}},
                "focus": max,
                "buffers": [env.path("missing.txt")],
            },
        });
        let path = private_state_dir(&env).join("1-2.json");
        fs::write(&path, state.to_string()).unwrap();
        let mut resumed = client(&env, &["--resume", path.to_str().unwrap()]);
        resumed.wait_for("b.txt");
        resumed.keys("jjkk<C-w>w<C-w>w<C-w>w");
        resumed.never_shows("went away");
        assert!(resumed.is_running(), "{session}: {}", resumed.screen());
    }
}

#[test]
fn auto_save_comes_back() {
    let env = split_env();
    let a = client(&env, &["--auto-save", "100", "a.txt"]);
    let path = wait_for_layout(&env, |layout| views(&layout["root"]) == ["a.txt"]);
    assert_eq!(read_state(&path).unwrap()["auto_save"], 100);
    drop(a);
    let resumed = resume_unchanged(&env, &path);
    resumed.keys("izap<esc>");
    wait_until("the edit to be saved", || {
        env.read("a.txt").starts_with("zap")
    });
}

/// As without resuming, a relative `--config` is resolved against `--working-dir`. The state
/// file has the resolved path.
#[test]
fn config_is_found_in_the_working_directory() {
    let env = split_env();
    env.write(
        "sub/editor.toml",
        "[editor]\nkitty-keyboard-protocol = \"disabled\"\nbufferline = \"always\"\n",
    );
    env.write("sub/a.txt", "one\n");
    let a = client(&env, &["-w", "sub", "-c", "editor.toml", "a.txt"]);
    let bufferline = |screen: &str| {
        screen
            .lines()
            .next()
            .is_some_and(|row| row.contains("a.txt"))
    };
    a.wait_until("the bufferline", bufferline);
    let path = wait_for_layout(&env, |layout| views(&layout["root"]) == ["a.txt"]);
    let state = read_state(&path).unwrap();
    let config = Path::new(state["config"].as_str().unwrap());
    assert_eq!(
        fs::canonicalize(config).unwrap(),
        fs::canonicalize(env.path("sub/editor.toml")).unwrap()
    );
    drop(a);
    let resumed = resume_unchanged(&env, &path);
    resumed.wait_until("the bufferline", bufferline);
}

/// Its files open in the editor and are deleted on quitting, so other users must not be able
/// to put files there.
#[test]
fn resume_directory_must_be_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let env = split_env();
    fs::create_dir_all(state_dir(&env)).unwrap();
    fs::set_permissions(state_dir(&env), fs::Permissions::from_mode(0o755)).unwrap();
    let mut a = client(&env, &["a.txt"]);
    let mut b = client(&env, &["--no-session", "a.txt"]);
    a.never_shows("never");
    assert!(state_files(&env).is_empty());
    assert!(reports(&a).is_empty() && reports(&b).is_empty());

    let path = state_dir(&env).join("1-2.json");
    fs::write(&path, "{}").unwrap();
    let mut shell = env.shell(&format!(
        "\"$HX\" --resume {}; echo exit=$?",
        path.display()
    ));
    shell.wait_for("exit=1");
    a.command("q");
    b.command("q");
    a.wait_exit();
    b.wait_exit();
    shell.wait_exit();
}

/// Another instance deletes a state file left unwritten for too long, as it would one from an
/// instance that the terminal never brought back.
#[test]
fn deleted_state_file_is_written_again() {
    let env = split_env();
    let a = client(&env, &["--no-session", "a.txt"]);
    let [path] = state_files(&env).try_into().unwrap();
    fs::remove_file(&path).unwrap();
    a.keys("<esc>");
    wait_until("the state file to be written again", || path.exists());
}

#[test]
fn missing_state_file_starts_as_usual() {
    let env = split_env();
    let path = state_dir(&env).join("1-2.json");
    let a = client(&env, &["--resume", path.to_str().unwrap()]);
    a.wait_for("[scratch]");
    wait_until("the state file", || path.exists());
}
