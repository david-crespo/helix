# Sessions

Several terminals can share one editor. Each `hx` in the session shows the
same open buffers, registers, language servers and diagnostics, while keeping
its own window layout, mode, working directory, environment and theme. Edit a
file in one terminal and the change shows up at once in every other terminal
that has it open.

Sessions are on by default: every `hx` started in a workspace joins the
editor already running there. Turn them off for every `hx` with the `session`
key in the `[editor]` section of your config:

```toml
[editor]
session = "off"
```

or for one `hx` with `--no-session`. `hx --config <file>` also runs a
separate editor, since a running one keeps the config it started with.

## How it works

The first `hx` in a session starts a server in the background and then
connects to it, like every later `hx` does. The server runs the editor and
draws to each connected terminal; the `hx` process in the terminal just waits
until you quit. When the last terminal quits, the server exits.

Unless told otherwise, `hx` joins the session for its workspace: the
nearest parent directory with a `.git`, `.jj`, `.svn` or `.helix` directory,
or the current directory if there is none. Every `hx` started inside one
repository shares an editor, and `hx` in another repository gets its own.
Setting `HELIX_SESSION` in the environment picks a session by name instead.

The server logs to `session-<name>.log` in Helix's cache directory, next to
`helix.log`.

## Command line flags

| Flag | Description |
| --- | --- |
| `-s`, `--session <name>` | Join the session with this name, starting it if needed. Works even when `session` is `"off"`. |
| `--no-session` | Run a separate editor instead of joining the session. |
| `--remote` | Open the files given on the command line in the session's most recently used terminal, then exit. Without a running session, the files open in this terminal as usual. |
| `--wait` | With `--remote`, exit once none of the files is shown in any terminal. Useful as `$EDITOR` for tools like `git commit`. |
| `--auto-save <ms>` | Auto-save the buffers this terminal has open: `<ms>` milliseconds after an edit, and when the terminal loses focus. An edit made in another terminal counts too, and when several terminals that auto-save have a buffer open, the shortest delay wins. Without a session, it's the same as setting `focus-lost = true` and `after-delay = { enable = true, timeout = <ms> }` under [`[editor.auto-save]`](./editor.md#editorauto-save-section). |

## Quitting

`:quit` and `:quit-all` close the views in the current terminal, and that
terminal's `hx` exits when its last view closes. The other terminals keep
running. Because buffers are shared, `:quit` only refuses over unsaved changes
that no other terminal still shows; `:quit!` drops those changes.

`:session-quit` closes every terminal in the session, refusing if any buffer
has unsaved changes, and `:session-quit!` does the same while ignoring them.

If a terminal goes away without quitting (the window closes, an SSH
connection drops), its unsaved changes stay in the session for the other
terminals. If it was the last one, the server exits and the changes are lost,
as they would be without sessions.

## What each terminal keeps to itself

- **Working directory.** `:cd` changes it for the current terminal only. File
  pickers, global search and shell commands use the working directory of the
  terminal they were started from.
- **Environment.** Shell commands (`:sh`, `|`, `!` and so on) run with the
  environment of the terminal they were run from, so tools like direnv work
  per terminal. So do Steel plugins: `env-var`, `maybe-get-env-var` and the
  processes `command` starts see the environment and working directory of the
  terminal the plugin code runs for. Language servers are shared, so they run with the environment
  the server started with, which is that of the session's first terminal.
- **Theme.** `:theme` changes the theme of the current terminal, and a theme
  with separate light and dark variants follows each terminal's own setting.
- **Messages.** The result of a command, such as a save, shows in the terminal
  that ran it. Messages nothing in particular asked for, such as one from a
  language server, show in every terminal. Those about a file changed on disk
  show in the terminals showing it, and the question whether to reload a
  buffer with unsaved changes is asked in one of them.
- **Clipboard.** Each terminal uses the clipboard provider a separate `hx`
  started there would: the one its config names, or else the one Helix finds
  in its environment. So a yank in a terminal over SSH, with no display or
  `pbcopy` on its `PATH`, sets that terminal's clipboard with an escape
  sequence, while one in a local terminal uses the system clipboard. Providers
  that run a command run it on the machine the server runs on.
- **Auto-save.** A terminal auto-saves the buffers it has open if it was
  started with `--auto-save` or `[editor.auto-save]` turns it on. An edit to
  such a buffer is saved after the delay whichever terminal made it, and the
  terminal saves them when it loses focus. Buffers that no auto-saving
  terminal has open are left alone.

Configuration, including `.helix/config.toml` in the workspace, is read when
the server starts and applies to every terminal.
