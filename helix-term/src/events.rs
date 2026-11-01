use helix_core::file_watcher::{FileSystemDidChange, WatchRootCrawled};
use helix_event::{events, register_event};
use helix_view::document::Mode;
use helix_view::events::{
    ConfigDidChange, DiagnosticsDidChange, DocumentChangedOnDisk, DocumentDidChange,
    DocumentDidClose, DocumentDidOpen, DocumentFocusLost, DocumentSaved, LanguageServerExited,
    LanguageServerInitialized, SelectionDidChange,
};
use helix_view::{ClientId, Editor};

use crate::commands;
use crate::keymap::MappableCommand;

events! {
    OnModeSwitch<'a, 'cx> { old_mode: Mode, new_mode: Mode, cx: &'a mut commands::Context<'cx> }
    PostInsertChar<'a, 'cx> { c: char, cx: &'a mut commands::Context<'cx> }
    PostCommand<'a, 'cx> { command: & 'a MappableCommand, cx: &'a mut commands::Context<'cx> }
    TerminalFocusGained<'a, 'cx> { cx: &'a mut commands::Context<'cx> }
    TerminalFocusLost<'a, 'cx> { cx: &'a mut commands::Context<'cx> }
    // A terminal joined or left a shared editor. The session's first client doesn't attach.
    ClientAttached<'a> { editor: &'a mut Editor, client: ClientId }
    ClientDetached<'a> { editor: &'a mut Editor, client: ClientId }
}

pub fn register() {
    register_event::<OnModeSwitch>();
    register_event::<PostInsertChar>();
    register_event::<PostCommand>();
    register_event::<TerminalFocusGained>();
    register_event::<TerminalFocusLost>();
    register_event::<ClientAttached>();
    register_event::<ClientDetached>();
    register_event::<DocumentDidOpen>();
    register_event::<DocumentDidChange>();
    register_event::<DocumentDidClose>();
    register_event::<DocumentFocusLost>();
    register_event::<DocumentSaved>();
    register_event::<DocumentChangedOnDisk>();
    register_event::<SelectionDidChange>();
    register_event::<DiagnosticsDidChange>();
    register_event::<LanguageServerInitialized>();
    register_event::<LanguageServerExited>();
    register_event::<ConfigDidChange>();
    register_event::<FileSystemDidChange>();
    register_event::<WatchRootCrawled>();
}
