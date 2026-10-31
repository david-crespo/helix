//! Acceptance criteria for running several `hx` clients against one shared editor.
//!
//! Each test describes behavior a user can observe. Some pass trivially against a
//! single-process Helix (two independent editors also have separate modes and working
//! directories); they guard against a shared implementation getting them wrong. The ones
//! that need sharing (shared buffers, one language server, `:session-quit`, resuming into
//! shared state) fail until it exists.
//!
//! Run with `cargo nextest run -p helix-term --features multi-client-tests --test multi_client`.
//! Set `HX_BIN` to check another build.

#![cfg(unix)]

#[cfg(feature = "integration")]
compile_error!("`integration` swaps the terminal for a test backend; run these tests without it");

mod harness;

use harness::{Env, ROWS};

// Shared state

#[test]
fn edits_show_up_in_other_clients_live() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    let a = env.client(&["a.txt"]);
    let b = env.client(&["a.txt"]);

    a.keys("izap<esc>");
    b.wait_for("zapone");
}

#[test]
fn new_client_sees_unsaved_edits() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    let a = env.client(&["a.txt"]);
    a.keys("izap<esc>");
    a.wait_for("zapone");

    let b = env.client(&["a.txt"]);
    b.wait_for("zapone");
    assert_eq!(env.read("a.txt"), "one\n", "nothing was saved");
}

#[test]
fn clients_share_one_language_server() {
    let env = Env::with_languages(
        r#"
[[language]]
name = "fake"
scope = "source.fake"
file-types = ["fake"]
language-servers = ["fake-ls"]

[language-server.fake-ls]
command = "{root}/fake-ls"
"#,
    );
    let log = env.root().join("fake-ls.log");
    // Records each start, then reads requests without answering them.
    env.script(
        "fake-ls",
        &format!(
            "#!/bin/sh\necho $$ >> '{}'\nexec cat > /dev/null\n",
            log.display()
        ),
    );
    env.write("x.fake", "x\n");
    let starts = || {
        std::fs::read_to_string(&log)
            .map(|log| log.lines().count())
            .unwrap_or(0)
    };

    let _a = env.client(&["x.fake"]);
    wait_until("the language server to start", || starts() == 1);
    let _b = env.client(&["x.fake"]);
    std::thread::sleep(harness::SETTLE);
    assert_eq!(
        starts(),
        1,
        "the second client started its own language server"
    );
}

// Per-client state

#[test]
fn mode_is_per_client() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    let a = env.client(&["a.txt"]);
    let b = env.client(&["a.txt"]);

    a.keys("i");
    a.wait_until("insert mode", |_| a.status_line().contains("INS"));
    b.never_shows("INS");
}

#[test]
fn working_directory_is_per_client() {
    let env = Env::new();
    env.write("p1/rel.txt", "from p1\n");
    env.write("p2/rel.txt", "from p2\n");
    let a = env.client_in(&env.path("p1"), &[]);
    let b = env.client_in(&env.path("p2"), &[]);

    // Relative paths resolve against the client's directory.
    a.command("o rel.txt");
    b.command("o rel.txt");
    a.wait_for("from p1");
    b.wait_for("from p2");

    // So do shell commands.
    a.command("sh basename \"$PWD\"");
    b.command("sh basename \"$PWD\"");
    a.wait_for("p1");
    b.wait_for("p2");
}

/// Completing a path in a scratch buffer lists the client's directory, not the server's.
#[test]
fn path_completion_lists_the_clients_directory() {
    let env = Env::new();
    env.write("p1/only-in-p1.txt", "");
    env.write("p2/only-in-p2.txt", "");
    let a = env.client_in(&env.path("p1"), &[]);
    let b = env.client_in(&env.path("p2"), &[]);

    a.keys("i./");
    a.wait_for("only-in-p1.txt");
    b.keys("i./");
    b.wait_for("only-in-p2.txt");
}

#[test]
fn piped_stdin_opens_only_in_that_client() {
    let env = Env::new();
    let a = env.shell("printf 'piped text' | \"$HX\"");
    a.wait_for("piped text");

    let b = env.client(&[]);
    b.never_shows("piped text");
}

#[test]
fn resize_affects_only_that_client() {
    let env = Env::new();
    let a = env.client(&[]);
    let b = env.client(&[]);

    a.resize(40, 100);
    a.wait_until("the status line to move to row 38", |_| {
        a.row(38).contains("NOR")
    });
    assert!(b.row(ROWS - 2).contains("NOR"), "{}", b.screen());
}

// Quitting

#[test]
fn quit_closes_only_the_calling_client() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    let mut a = env.client(&["a.txt"]);
    let mut b = env.client(&["a.txt"]);

    a.command("q");
    assert_eq!(a.wait_exit(), 0);
    assert!(b.is_running());
    b.keys("izap<esc>");
    b.wait_for("zapone");
}

/// Quitting is refused when it would leave an edited buffer that no client shows.
#[test]
fn quit_refuses_when_it_would_orphan_an_edit() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    env.write("b.txt", "two\n");
    let mut a = env.client(&["a.txt"]);
    let _b = env.client(&["b.txt"]);

    a.keys("izap<esc>");
    a.command("q");
    a.wait_for("unsaved");
    assert!(a.is_running());
}

/// Edits still shown in another client stay visible there, so quitting is fine.
#[test]
fn quit_allows_edits_another_client_still_shows() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    let mut a = env.client(&["a.txt"]);
    let b = env.client(&["a.txt"]);

    a.keys("izap<esc>");
    b.wait_for("zapone");
    a.command("q");
    assert_eq!(a.wait_exit(), 0);
    b.keys("A!<esc>");
    b.wait_for("zapone!");
}

#[test]
fn quit_bang_reverts_orphaned_edits() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    env.write("b.txt", "two\n");
    let mut a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    a.keys("izap<esc>");
    a.wait_for("zapone");
    a.command("q!");
    assert_eq!(a.wait_exit(), 0);
    b.command("o a.txt");
    b.wait_for("one");
    b.never_shows("zapone");
}

/// Closing a buffer closes it for everyone, as with splits today, and the other client keeps
/// working.
#[test]
fn closing_a_buffer_in_one_client_fixes_up_views_in_others() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    let a = env.client(&["a.txt"]);
    let mut b = env.client(&["a.txt"]);

    a.command("bc");
    b.wait_until("a.txt to close", |screen| !screen.contains("a.txt"));
    assert!(b.is_running());
    b.keys("ihello<esc>");
    b.wait_for("hello");
}

/// When a client leaves, the buffers only it had open close, so a later `hx` reads them fresh.
/// Buffers another client has open stay, even ones it isn't showing.
#[test]
fn leaving_closes_buffers_only_that_client_had_open() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    env.write("b.txt", "two\n");
    env.write("c.txt", "three\n");
    let mut a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt", "c.txt"]);
    b.keys("<space>b");
    b.wait_for("a.txt");
    b.keys("<esc>");

    a.command("q");
    assert_eq!(a.wait_exit(), 0);
    b.keys("<space>b");
    b.wait_for("c.txt");
    b.never_shows("a.txt");
}

/// `git commit` with Helix as `$EDITOR`: git rewrites the file between commits, and the next
/// client has to see the new contents and be able to save.
#[test]
fn reopening_a_rewritten_file_shows_new_contents() {
    let env = Env::new();
    env.write("MSG", "\n");
    let mut a = env.client(&["MSG"]);
    a.keys("ifirst<esc>");
    a.command("wq");
    assert_eq!(a.wait_exit(), 0);

    env.write("MSG", "second\n");
    let mut b = env.client(&["MSG"]);
    b.wait_for("second");
    b.keys("ggiedited <esc>");
    b.command("wq");
    assert_eq!(b.wait_exit(), 0);
    assert_eq!(env.read("MSG"), "edited second\n");
}

/// The same, while another client keeps the file open: an unmodified buffer reloads when a new
/// client opens its file and the file changed.
#[test]
fn opening_a_rewritten_file_reloads_it() {
    let env = Env::new();
    env.write("MSG", "first\n");
    let keeper = env.client(&["MSG"]);
    keeper.wait_for("first");

    env.write("MSG", "second\n");
    let b = env.client(&["MSG"]);
    b.wait_for("second");
    keeper.wait_for("second");
}

#[test]
fn exit_code_reaches_the_client() {
    let env = Env::new();
    let mut a = env.client(&[]);
    let mut b = env.client(&[]);

    a.command("cq 3");
    assert_eq!(a.wait_exit(), 3);
    assert!(b.is_running());
}

#[test]
fn session_quit_stops_every_client() {
    let env = Env::new();
    let mut a = env.client(&[]);
    let mut b = env.client(&[]);

    a.command("session-quit");
    a.wait_exit();
    b.wait_exit();
}

/// Every client's edits would be lost, so `:session-quit` refuses over any of them.
#[test]
fn session_quit_refuses_over_another_clients_edits() {
    let env = Env::new();
    env.write("b.txt", "two\n");
    let mut a = env.client(&[]);
    let mut b = env.client(&["b.txt"]);

    b.keys("izap<esc>");
    b.wait_for("zaptwo");
    a.command("session-quit");
    a.wait_for("unsaved");
    assert!(a.is_running() && b.is_running());

    a.command("session-quit!");
    a.wait_exit();
    b.wait_exit();
    assert_eq!(env.read("b.txt"), "two\n");
}

// Opening files from a shell

/// `$EDITOR` use: `hx file` in a pane blocks until that client quits and reports its exit code,
/// even while another client holds the session.
#[test]
fn editor_command_blocks_until_quit() {
    let env = Env::new();
    env.write("MSG", "\n");
    let _a = env.client(&[]);
    let shell = env.shell("\"$HX\" MSG; echo \"exit=$?\"; sleep 30");
    shell.wait_for("NOR");
    shell.keys("imessage<esc>");
    shell.command("wq");
    shell.wait_for("exit=0");
    assert_eq!(env.read("MSG"), "message\n");
}

#[test]
fn remote_open_lands_in_the_running_client() {
    let env = Env::new();
    env.write("b.txt", "from b\n");
    let a = env.client(&[]);
    let shell = env.shell("\"$HX\" --remote b.txt; echo \"exit=$?\"; sleep 30");
    shell.wait_for("exit=0");
    a.wait_for("from b");
}

/// With nothing running to open the files in, `--remote` opens them in its own terminal.
#[test]
fn remote_without_a_session_opens_here() {
    let env = Env::new();
    env.write("b.txt", "from b\n");
    let shell = env.shell("\"$HX\" --remote b.txt");
    shell.wait_for("from b");
}

/// A `--remote` that reaches a new server before the client that started it waits for that
/// client, rather than taking the server down. The server is started by hand, as `hx` would
/// start it, so the `--remote` can get there first.
#[test]
fn remote_reaching_a_new_server_first_opens_in_its_first_client() {
    let env = Env::new();
    env.write("b.txt", "from b\n");
    let shell = env.shell(
        "\"$HX\" --session-server s >/dev/null 2>&1 &
        until [ -S \"$XDG_RUNTIME_DIR/helix/s.sock\" ]; do sleep 0.02; done
        \"$HX\" --session s --remote b.txt &
        sleep 0.5
        \"$HX\" --session s",
    );
    shell.wait_for("from b");
}

/// `hx --remote --wait` as `$EDITOR`: it returns once the file is no longer shown.
#[test]
fn remote_wait_returns_once_the_file_is_closed() {
    let env = Env::new();
    env.write("MSG", "message\n");
    let a = env.client(&[]);
    let shell = env.shell("\"$HX\" --remote --wait MSG; echo \"exit=$?\"; sleep 30");
    a.wait_for("message");
    shell.never_shows("exit=");

    a.command("bc");
    shell.wait_for("exit=0");
}

/// It exits with the code of the client the file opened in, so `:cq` aborts a commit.
#[test]
fn remote_wait_exits_with_the_clients_code() {
    let env = Env::new();
    env.write("MSG", "message\n");
    let _keeper = env.client(&[]);
    let mut a = env.client(&[]);
    let shell = env.shell("\"$HX\" --remote --wait MSG; echo \"exit=$?\"; sleep 30");
    a.wait_for("message");

    a.command("cq 5");
    assert_eq!(a.wait_exit(), 5);
    shell.wait_for("exit=5");
}

// Losing a terminal

/// Closing the last pane ends the session, as closing the terminal ends a standalone editor:
/// the next `hx` starts fresh.
#[test]
fn hung_up_last_client_ends_the_session() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    let mut a = env.client(&["a.txt"]);
    a.keys("izap<esc>");
    a.wait_for("zapone");
    a.hang_up();
    a.wait_exit();

    let b = env.client(&["a.txt"]);
    b.wait_for("one");
    b.never_shows("zap");
    assert_eq!(env.read("a.txt"), "one\n");
}

/// Highlight scopes are shared, but each client can have its own theme, which looks syntax
/// colors up by them.
#[test]
fn syntax_colors_follow_each_clients_theme() {
    use vt100::Color;
    let env = Env::new();
    env.theme(
        "red",
        "\"ui.selection\" = { bg = \"gray\" }\nkeyword = \"red\"\n",
    );
    env.theme(
        "blue",
        "\"ui.selection\" = { bg = \"gray\" }\ncomment = \"green\"\nconstant = \"green\"\nkeyword = \"blue\"\n",
    );
    env.write("x.rs", "fn main() {}\n");
    let a = env.client(&["x.rs"]);
    let b = env.client(&["x.rs"]);

    a.command("theme red");
    b.command("theme blue");
    b.wait_until("blue keywords", |_| b.color_of("fn") == Some(Color::Idx(4)));
    a.keys("j");
    a.wait_until("red keywords", |_| a.color_of("fn") == Some(Color::Idx(1)));
}

/// A client joining a session starts with the theme of the client last used, such as one set
/// with `:theme` or by Steel config, but not with a theme that client is only previewing.
#[test]
fn joining_client_gets_the_active_clients_theme() {
    use vt100::Color;
    let env = Env::new();
    env.theme(
        "red",
        "\"ui.selection\" = { bg = \"gray\" }\nkeyword = \"red\"\n",
    );
    env.theme(
        "blue",
        "\"ui.selection\" = { bg = \"gray\" }\nkeyword = \"blue\"\n",
    );
    env.write("x.rs", "fn main() {}\n");
    let a = env.client(&["x.rs"]);
    a.command("theme red");
    a.wait_until("red keywords", |_| a.color_of("fn") == Some(Color::Idx(1)));

    let b = env.client(&["x.rs"]);
    b.wait_until("red keywords", |_| b.color_of("fn") == Some(Color::Idx(1)));

    a.keys(":theme blue");
    a.wait_until("blue preview", |_| a.color_of("fn") == Some(Color::Idx(4)));
    let c = env.client(&["x.rs"]);
    c.wait_until("red keywords", |_| c.color_of("fn") == Some(Color::Idx(1)));
}

// Environment

/// Shell commands see the environment of the client that ran them, so direnv-style
/// per-directory variables work.
#[test]
fn shell_commands_use_the_client_environment() {
    let env = Env::new();
    let a = env.client_with(&env.project(), &[("WHO", "alpha")], &[]);
    let b = env.client_with(&env.project(), &[("WHO", "bravo")], &[]);

    a.command("sh echo $WHO");
    b.command("sh echo $WHO");
    a.wait_for("alpha");
    b.wait_for("bravo");
}

// Suspending

#[test]
fn suspending_a_client_leaves_the_others_running() {
    let env = Env::new();
    env.write("a.txt", "one\n");
    // Job control needs a shell: a session leader's process group ignores SIGTSTP.
    let a = env.interactive_shell();
    a.keys("\"$HX\" a.txt<ret>");
    a.wait_for("NOR");
    let b = env.client(&["a.txt"]);

    a.keys("<C-z>");
    a.wait_for("Stopped");
    b.keys("izap<esc>");
    b.wait_for("zapone");

    // Coming back shows what happened while it was away.
    a.keys("fg<ret>");
    a.wait_for("zapone");
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !done() {
        assert!(
            start.elapsed() < harness::TIMEOUT,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
