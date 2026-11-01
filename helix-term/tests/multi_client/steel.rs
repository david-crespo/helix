//! Steel plugins in a shared editor: plugin code acts as the client that caused it.

use crate::harness::Env;

/// An environment whose `init.scm` is `init`, after requiring the modules the tests use.
fn steel_env(init: &str) -> Env {
    let env = Env::new();
    env.config_file("helix.scm", "");
    env.config_file(
        "init.scm",
        &format!(
            r#"(require "helix/editor.scm")
(require "helix/misc.scm")
(require (prefix-in helix.static. "helix/static.scm"))
(require (only-in "helix/keymaps.scm" keymap))
{init}"#
        ),
    );
    env.write("a.txt", "a\n");
    env.write("b.txt", "b\n");
    env
}

#[test]
fn keybinding_runs_as_the_invoking_client() {
    let env = steel_env(
        r#"(define (insert-here) (helix.static.insert_string "K"))
(keymap (global) (normal (C-t ":insert-here")))"#,
    );
    let a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    b.keys("<C-t>");
    b.wait_for("Kb");
    a.never_shows("Ka");
}

/// A timer acts as the client that set it, even once another client has taken focus.
#[test]
fn timer_runs_as_the_client_that_set_it() {
    let env = steel_env(
        r#"(define (insert-later)
  (enqueue-thread-local-callback-with-delay 500 (lambda () (helix.static.insert_string "T")))
  (set-status! "timer set"))"#,
    );
    let a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    a.command("insert-later");
    a.wait_for("timer set");
    b.keys("l");
    a.wait_for("Ta");
    b.never_shows("Tb");
}

/// A hook runs as the client whose edit fired it: here a timer of A's, while B has focus.
#[test]
fn hook_runs_as_the_client_that_caused_the_event() {
    let env = steel_env(
        r#"(define (insert-later)
  (enqueue-thread-local-callback-with-delay 500 (lambda () (helix.static.insert_string "T")))
  (set-status! "timer set"))
(register-hook 'document-changed (lambda (doc old-text) (set-status! "hook ran here")))"#,
    );
    let a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    a.command("insert-later");
    a.wait_for("timer set");
    b.keys("l");
    a.wait_for("Ta");
    a.wait_for("hook ran here");
    b.never_shows("hook ran here");
}

#[test]
fn client_api_describes_each_clients_layout() {
    let env = steel_env(
        r#"(define (describe-client)
  (set-status!
    (string-append
      "views=" (number->string (length (client-views (current-client))))
      " owns-focus=" (if (equal? (view->client (editor-focus)) (current-client)) "yes" "no")
      " clients=" (number->string (length (editor-clients))))))"#,
    );
    let a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    a.command("vsplit");
    a.command("describe-client");
    a.wait_for("views=2 owns-focus=yes clients=2");
    b.command("describe-client");
    b.wait_for("views=1 owns-focus=yes clients=2");
}

/// `with-client` switches clients for its thunk, and back afterwards even if the thunk fails.
#[test]
fn with_client_acts_as_another_client() {
    let env = steel_env(
        r#"(define (in-others thunk)
  (for-each (lambda (client) (unless (equal? client (current-client)) (with-client client thunk)))
            (editor-clients)))
(define (insert-in-others) (in-others (lambda () (helix.static.insert_string "W"))))
(define (fail-in-others)
  (with-handler (lambda (err) (helix.static.insert_string "C"))
                (in-others (lambda () (error "failed elsewhere")))))"#,
    );
    let a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    a.command("insert-in-others");
    b.wait_for("Wb");
    a.never_shows("Wa");

    a.command("fail-in-others");
    a.wait_for("Ca");
    b.never_shows("CWb");
}

/// Work no client caused, here a thread handing back to the editor, is still for no client after
/// a `with-client`, so its messages go to every client.
#[test]
fn with_client_restores_running_as_no_client() {
    let env = steel_env(
        r#"(require "helix/ext.scm")
(define (from-thread)
  (spawn-native-thread
    (lambda ()
      (hx.with-context
        (lambda ()
          (with-client (car (editor-clients)) (lambda () void))
          (set-status! "back from the thread"))))))"#,
    );
    let a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    a.command("from-thread");
    a.wait_for("back from the thread");
    b.wait_for("back from the thread");
}

#[test]
fn broadcast_status_reaches_every_client() {
    let env = steel_env(r#"(define (shout) (broadcast-status! "hello everyone"))"#);
    let a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    a.command("shout");
    a.wait_for("hello everyone");
    b.wait_for("hello everyone");
}

#[test]
fn client_hooks_run_as_clients_come_and_go() {
    let env = steel_env(
        r#"(register-hook 'client-attached (lambda (client) (set-status! "attached")))
(register-hook 'client-detached (lambda (client) (set-status! "detached")))"#,
    );
    let a = env.client(&["a.txt"]);
    let mut b = env.client(&["b.txt"]);

    b.wait_for("attached");
    a.never_shows("attached");
    b.command("q");
    b.wait_exit();
    a.wait_for("detached");
}

/// Ctrl-C stops a runaway command in whichever client ran it.
#[test]
fn ctrl_c_interrupts_a_command_from_any_client() {
    let env = steel_env("(define (spin) (let loop () (loop)))");
    let _a = env.client(&["a.txt"]);
    let b = env.client(&["b.txt"]);

    b.command("spin");
    std::thread::sleep(crate::harness::SETTLE);
    b.keys("<C-c>");
    b.keys("ix<esc>");
    b.wait_for("xb");
}

/// Ctrl-C while a command acts as another client stops it, and the client that ran it goes on
/// as itself.
#[test]
fn ctrl_c_inside_with_client_switches_back() {
    let env = steel_env(
        r#"(define (spin-as-other)
  (define other (car (filter (lambda (client) (not (equal? client (current-client)))) (editor-clients))))
  (with-client other (lambda () (let loop () (loop)))))"#,
    );
    let mut a = env.client(&["a.txt"]);
    let mut b = env.client(&["b.txt"]);

    b.command("spin-as-other");
    std::thread::sleep(crate::harness::SETTLE);
    b.keys("<C-c>");
    b.keys("ix<esc>");
    b.wait_for("xb");
    a.never_shows("xa");
    assert!(a.is_running() && b.is_running());
}

/// `with-client` gives another client's mode and focused view, from which a plugin can tell
/// whether someone is typing in a document.
#[test]
fn with_client_sees_another_clients_mode_and_focus() {
    let env = steel_env(
        r#"(define (describe-others)
  (define here (editor->doc-id (editor-focus)))
  (define others
    (map (lambda (client)
           (with-client client
             (lambda ()
               (string-append (if (equal? (editor-mode) (string->editor-mode "insert")) "inserting" "not-inserting")
                              (if (equal? (editor->doc-id (editor-focus)) here) "-here" "-elsewhere")))))
         (filter (lambda (client) (not (equal? client (current-client)))) (editor-clients))))
  (set-status! (string-append "others: " (apply string-append others)
                              (if (equal? (editor-mode) (string->editor-mode "normal")) " me: normal" " me: ?"))))"#,
    );
    let a = env.client(&["a.txt"]);
    let b = env.client(&["a.txt"]);

    a.keys("ix");
    b.wait_for("xa");
    b.command("describe-others");
    b.wait_for("others: inserting-here me: normal");
    a.keys("<esc>");
    b.command("describe-others");
    b.wait_for("others: not-inserting-here me: normal");
    a.keys(":open b.txt<ret>");
    a.wait_for("b.txt");
    b.command("describe-others");
    b.wait_for("others: not-inserting-elsewhere me: normal");
}

/// Steel code sees the environment and working directory of the client it runs as, as shell
/// commands do: in what it reads and in the processes it starts.
#[test]
fn steel_uses_the_client_environment() {
    let env = steel_env(
        r#"(define (who)
  (let* ([child (Ok->value (spawn-process (with-stdout-piped
           (command "sh" (list "-c" "printf '%s in %s' \"$WHO\" \"$(basename \"$(pwd -P)\")\"")))))]
         [spawned (read-port-to-string (child-stdout child))])
    (set-status! (string-append (Ok->value (maybe-get-env-var "WHO")) " " (env-var "WHO")
                                ", spawned " spawned))))"#,
    );
    env.write("sub/c.txt", "c\n");
    let a = env.client_with(&env.project(), &[("WHO", "alpha")], &["a.txt"]);
    let b = env.client_with(&env.path("sub"), &[("WHO", "bravo")], &["c.txt"]);

    a.command("who");
    a.wait_for("alpha alpha, spawned alpha in project");
    b.command("who");
    b.wait_for("bravo bravo, spawned bravo in sub");
}
