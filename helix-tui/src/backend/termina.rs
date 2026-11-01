use std::{
    fs::File,
    io::{self, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
};

use helix_view::{
    editor::KittyKeyboardProtocolConfig,
    graphics::{CursorKind, Rect, UnderlineStyle},
    theme::{self, Color, Modifier},
};
use termina::{
    escape::{
        csi::{self, Csi, SgrAttributes, SgrModifiers},
        dcs::{self, Dcs},
        osc::{self, Osc},
    },
    style::{CursorStyle, RgbColor},
    Event, OneBased, PlatformTerminal, Terminal as _, WindowSize,
};

use crate::{buffer::Cell, terminal::Config};

use super::Backend;

// These macros are helpers to set/unset modes like bracketed paste or enter/exit the alternate
// screen.
macro_rules! decset {
    ($mode:ident) => {
        Csi::Mode(csi::Mode::SetDecPrivateMode(csi::DecPrivateMode::Code(
            csi::DecPrivateModeCode::$mode,
        )))
    };
}
macro_rules! decreset {
    ($mode:ident) => {
        Csi::Mode(csi::Mode::ResetDecPrivateMode(csi::DecPrivateMode::Code(
            csi::DecPrivateModeCode::$mode,
        )))
    };
}

fn term_program(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    // Some terminals don't set $TERM_PROGRAM
    env("TERM_PROGRAM").or_else(|| env("TERM"))
}
fn vte_version(env: &dyn Fn(&str) -> Option<String>) -> Option<usize> {
    env("VTE_VERSION")?.parse().ok()
}

#[derive(Debug, Default, Clone, Copy)]
struct Capabilities {
    kitty_keyboard: KittyKeyboardSupport,
    synchronized_output: bool,
    true_color: bool,
    extended_underlines: bool,
    /// OSC11 / OSC111 - change the terminal's background color.
    dynamic_background_color: bool,
    theme_mode: Option<theme::Mode>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum KittyKeyboardSupport {
    /// The terminal doesn't support the protocol.
    #[default]
    None,
    /// The terminal supports the protocol but we haven't checked yet whether it has full or
    /// partial support for the flags we require.
    Some,
    /// The terminal only supports some of the flags we require.
    Partial,
    /// The terminal supports all flags require.
    Full,
}

#[derive(Debug)]
pub struct TerminaBackend {
    terminal: PlatformTerminal,
    config: Config,
    capabilities: Capabilities,
    reset_cursor_command: String,
    is_synchronized_output_set: bool,
    /// The requested terminal background color, set in `Self::set_background_color`.
    background_color: Option<RgbColor>,
    /// The terminal emulator's background color. This is queried when claiming the terminal so
    /// that custom colors set outside of Helix with OSC11 are restored when Helix exits.
    original_background_color: Option<RgbColor>,
    /// Whether the terminal is set up for us. While it isn't, another process may be using it,
    /// so we leave it alone.
    claimed: bool,
    /// What we write goes through this rather than straight to `terminal`, if set.
    writer: Option<TerminalWriter>,
}

impl TerminaBackend {
    pub fn new(config: Config) -> io::Result<Self> {
        let mut backend = Self::with_terminal(PlatformTerminal::new()?, config, &|name| {
            std::env::var(name).ok()
        })?;
        backend.set_panic_hook();
        Ok(backend)
    }

    /// Claims `terminal`, which need not be this process's own. `env` looks up variables in the
    /// environment of the program the terminal belongs to, which describe the terminal.
    pub fn with_terminal(
        mut terminal: PlatformTerminal,
        config: Config,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> io::Result<Self> {
        use std::time::{Duration, Instant};

        // Colibri "midnight"
        const TEST_COLOR: RgbColor = RgbColor::new(59, 34, 76);

        terminal.enter_raw_mode()?;

        let mut capabilities = Capabilities::default();
        let mut original_background_color = None;
        let start = Instant::now();

        // HACK: emitting OSC11 / OSC111 seems to break SGR and cause flickering in tmux.
        capabilities.dynamic_background_color = env("TMUX").is_none();

        capabilities.kitty_keyboard = match config.kitty_keyboard_protocol {
            KittyKeyboardProtocolConfig::Disabled => KittyKeyboardSupport::None,
            KittyKeyboardProtocolConfig::Enabled => KittyKeyboardSupport::Full,
            KittyKeyboardProtocolConfig::Auto => {
                write!(terminal, "{}", Csi::Keyboard(csi::Keyboard::QueryFlags))?;
                KittyKeyboardSupport::None
            }
        };

        // Many terminal extensions can be detected by querying the terminal for the state of the
        // extension and then sending a request for the primary device attributes (which is
        // consistently supported by all terminals). If we receive the status of the feature (for
        // example the current Kitty keyboard flags) then we know that the feature is supported.
        // If we only receive the device attributes then we know it is not.
        write!(
            terminal,
            "{}{}{}{}{}{}{}{}",
            // Synchronized output
            Csi::Mode(csi::Mode::QueryDecPrivateMode(csi::DecPrivateMode::Code(
                csi::DecPrivateModeCode::SynchronizedOutput
            ))),
            // Mode 2031 theme updates. Query the current theme.
            Csi::Mode(csi::Mode::QueryTheme),
            // True color and while we're at it, extended underlines:
            // <https://github.com/termstandard/colors?tab=readme-ov-file#querying-the-terminal>
            Csi::Sgr(csi::Sgr::Background(TEST_COLOR.into())),
            Csi::Sgr(csi::Sgr::UnderlineColor(TEST_COLOR.into())),
            Dcs::Request(dcs::DcsRequest::GraphicRendition),
            Csi::Sgr(csi::Sgr::Reset),
            Osc::ChangeDynamicColors(
                osc::DynamicColorNumber::TextBackgroundColor,
                vec![osc::ColorOrQuery::Query]
            ),
            // Finally request the primary device attributes
            Csi::Device(csi::Device::RequestPrimaryDeviceAttributes),
        )?;
        terminal.flush()?;

        let device_attributes = |event: &Event| {
            matches!(
                event,
                Event::Csi(Csi::Device(csi::Device::DeviceAttributes(_)))
            )
        };
        // TODO: tune this poll constant? Does it need to be longer when on an SSH connection?
        let poll_duration = Duration::from_millis(100);
        if terminal.poll(device_attributes, Some(poll_duration))? {
            while terminal.poll(Event::is_escape, Some(Duration::ZERO))? {
                match terminal.read(Event::is_escape)? {
                    Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(_))) => {
                        capabilities.kitty_keyboard = KittyKeyboardSupport::Some;
                    }
                    Event::Csi(Csi::Mode(csi::Mode::ReportDecPrivateMode {
                        mode: csi::DecPrivateMode::Code(csi::DecPrivateModeCode::SynchronizedOutput),
                        setting: csi::DecModeSetting::Set | csi::DecModeSetting::Reset,
                    })) => {
                        capabilities.synchronized_output = true;
                    }
                    Event::Csi(Csi::Mode(csi::Mode::ReportTheme(mode))) => {
                        capabilities.theme_mode = Some(mode.into());
                    }
                    Event::Osc(Osc::ChangeDynamicColors(
                        osc::DynamicColorNumber::TextBackgroundColor,
                        colors,
                    )) => {
                        if let Some(osc::ColorOrQuery::Color(color)) = colors.first() {
                            original_background_color = Some(*color);
                        }
                    }
                    Event::Dcs(dcs::Dcs::Response {
                        value: dcs::DcsResponse::GraphicRendition(sgrs),
                        ..
                    }) => {
                        capabilities.true_color =
                            sgrs.contains(&csi::Sgr::Background(TEST_COLOR.into()));
                        capabilities.extended_underlines =
                            sgrs.contains(&csi::Sgr::UnderlineColor(TEST_COLOR.into()));
                    }
                    _ => (),
                }
            }

            let end = Instant::now();
            log::debug!(
                "Detected terminal capabilities in {:?}: {capabilities:?}",
                end.duration_since(start)
            );
        } else {
            log::debug!("Failed to detect terminal capabilities within {poll_duration:?}. Using default capabilities only");
        }

        capabilities.extended_underlines |= config.force_enable_extended_underlines;

        let mut reset_cursor_command = String::new();
        if let Some(Ok(t)) = env("TERM").map(|term| termini::TermInfo::from_name(&term)) {
            capabilities.extended_underlines |= t.extended_cap("Smulx").is_some()
                || t.extended_cap("Su").is_some()
                || vte_version(env) >= Some(5102)
                // HACK: once WezTerm can support DECRQSS/DECRPSS for SGR we can remove this line.
                // <https://github.com/wezterm/wezterm/pull/6856>
                || matches!(term_program(env).as_deref(), Some("WezTerm"));

            if let Some(termini::Value::Utf8String(se_str)) = t.extended_cap("Se") {
                reset_cursor_command.push_str(se_str);
            };
            reset_cursor_command.push_str(
                t.utf8_string_cap(termini::StringCapability::CursorNormal)
                    .unwrap_or(""),
            );
            log::debug!(
                "Cursor reset escape sequence detected from terminfo: {reset_cursor_command:?}"
            );
        } else {
            log::debug!("terminfo could not be read, using default cursor reset escape sequence: {reset_cursor_command:?}");
        }
        reset_cursor_command
            .push_str(&Csi::Cursor(csi::Cursor::CursorStyle(CursorStyle::Default)).to_string());

        terminal.enter_cooked_mode()?;

        Ok(Self {
            terminal,
            config,
            capabilities,
            reset_cursor_command,
            is_synchronized_output_set: false,
            background_color: None,
            original_background_color,
            claimed: false,
            writer: None,
        })
    }

    /// Resets the terminal eagerly on a panic. If we relied on `Drop` instead, the backtrace
    /// would be lost because it is printed before we clear and exit the alternate screen. Only
    /// for the process's own terminal: one passed in from elsewhere is its owner's to restore.
    fn set_panic_hook(&mut self) {
        let hook_reset_cursor_command = self.reset_cursor_command.clone();
        self.terminal.set_panic_hook(move |term| {
            let _ = write!(
                term,
                "{}{}{}{}{}{}{}{}{}{}{}{}",
                Csi::Keyboard(csi::Keyboard::PopFlags(1)),
                decreset!(MouseTracking),
                decreset!(ButtonEventMouse),
                decreset!(AnyEventMouse),
                decreset!(RXVTMouse),
                decreset!(SGRMouse),
                &hook_reset_cursor_command,
                decreset!(BracketedPaste),
                decreset!(FocusTracking),
                Osc::ResetDynamicColor(osc::DynamicColorNumber::TextBackgroundColor),
                Csi::Edit(csi::Edit::EraseInDisplay(csi::EraseInDisplay::EraseDisplay)),
                decreset!(ClearAndEnableAlternateScreen),
            );
        });
    }

    pub fn terminal(&self) -> &PlatformTerminal {
        &self.terminal
    }

    /// Writes through `writer` from now on, rather than straight to the terminal.
    pub fn set_writer(&mut self, writer: TerminalWriter) {
        self.writer = Some(writer);
    }

    /// Whether the terminal is behind on what was written to it, so drawing more now would
    /// only add to what it has to catch up on.
    pub fn is_behind(&self) -> bool {
        self.writer.as_ref().is_some_and(TerminalWriter::is_behind)
    }

    /// Runs `f` once what was written so far has reached the terminal, or failed to.
    pub fn then(&mut self, f: impl FnOnce() + Send + 'static) {
        let _ = self.out().flush();
        match &self.writer {
            Some(writer) => writer.then(f),
            None => f(),
        }
    }

    fn out(&mut self) -> &mut dyn Write {
        match &mut self.writer {
            Some(writer) => writer,
            None => &mut self.terminal,
        }
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        if self.config.enable_mouse_capture {
            write!(
                self.out(),
                "{}{}{}{}{}",
                decset!(MouseTracking),
                decset!(ButtonEventMouse),
                decset!(AnyEventMouse),
                decset!(RXVTMouse),
                decset!(SGRMouse),
            )?;
        }
        Ok(())
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        // If we don't do this, then disabling mouse support
        // at runtime fails, since mouse capture is now "disabled",
        // and then on exit the terminal is still capturing things.
        if self.config.enable_mouse_capture || cfg!(feature = "steel") {
            write!(
                self.out(),
                "{}{}{}{}{}",
                decreset!(MouseTracking),
                decreset!(ButtonEventMouse),
                decreset!(AnyEventMouse),
                decreset!(RXVTMouse),
                decreset!(SGRMouse),
            )?;
        }
        Ok(())
    }

    fn reset_background_color(&mut self) -> io::Result<()> {
        let reset = match self.original_background_color {
            Some(color) => Osc::ChangeDynamicColors(
                osc::DynamicColorNumber::TextBackgroundColor,
                vec![color.into()],
            ),
            None => Osc::ResetDynamicColor(osc::DynamicColorNumber::TextBackgroundColor),
        };
        write!(self.out(), "{reset}")
    }

    /// Undoes what claiming the terminal set up.
    fn reset(&mut self) -> io::Result<()> {
        self.disable_extensions()?;
        self.disable_mouse_capture()?;
        let reset_cursor = self.reset_cursor_command.clone();
        write!(
            self.out(),
            "{}{}{}{}",
            &reset_cursor,
            decreset!(BracketedPaste),
            decreset!(FocusTracking),
            decreset!(ClearAndEnableAlternateScreen),
        )?;
        if self.background_color.is_some() {
            self.reset_background_color()?;
        }
        Ok(())
    }

    fn enable_extensions(&mut self) -> io::Result<()> {
        const KEYBOARD_FLAGS: csi::KittyKeyboardFlags =
            csi::KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES
                .union(csi::KittyKeyboardFlags::REPORT_ALTERNATE_KEYS);

        match self.capabilities.kitty_keyboard {
            KittyKeyboardSupport::None | KittyKeyboardSupport::Partial => (),
            KittyKeyboardSupport::Full => {
                write!(
                    self.out(),
                    "{}",
                    Csi::Keyboard(csi::Keyboard::PushFlags(KEYBOARD_FLAGS))
                )?;
            }
            KittyKeyboardSupport::Some => {
                write!(
                    self.out(),
                    "{}{}",
                    // Enable the flags we need.
                    Csi::Keyboard(csi::Keyboard::PushFlags(KEYBOARD_FLAGS)),
                    // Then request the current flags. We need to check if the terminal enabled
                    // all of the flags we require.
                    Csi::Keyboard(csi::Keyboard::QueryFlags),
                )?;
                self.out().flush()?;

                let reports_flags = |event: &Event| {
                    matches!(
                        event,
                        Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(_)))
                    )
                };
                // Not forever: a terminal that never answers would hold up whatever else this
                // thread does, which in a session's server is every client.
                let answered = self
                    .terminal
                    .poll(reports_flags, Some(std::time::Duration::from_secs(1)))?;
                let flags = match answered.then(|| self.terminal.read(reports_flags)) {
                    Some(event) => match event? {
                        Event::Csi(Csi::Keyboard(csi::Keyboard::ReportFlags(flags))) => Some(flags),
                        _ => unreachable!(),
                    },
                    None => None,
                };
                if flags != Some(KEYBOARD_FLAGS) {
                    log::info!("Turning off enhanced keyboard support because the terminal enabled different flags. Requested {KEYBOARD_FLAGS:?} but got {flags:?}");
                    write!(self.out(), "{}", Csi::Keyboard(csi::Keyboard::PopFlags(1)))?;
                    self.out().flush()?;
                    self.capabilities.kitty_keyboard = KittyKeyboardSupport::Partial;
                } else {
                    log::debug!(
                        "The terminal fully supports the requested keyboard enhancement flags"
                    );
                    self.capabilities.kitty_keyboard = KittyKeyboardSupport::Full;
                }
            }
        }

        if self.capabilities.theme_mode.is_some() {
            // Enable mode 2031 theme mode notifications:
            write!(self.out(), "{}", decset!(Theme))?;
        }

        Ok(())
    }

    fn disable_extensions(&mut self) -> io::Result<()> {
        if self.capabilities.kitty_keyboard == KittyKeyboardSupport::Full {
            write!(self.out(), "{}", Csi::Keyboard(csi::Keyboard::PopFlags(1)))?;
        }

        if self.capabilities.theme_mode.is_some() {
            // Mode 2031 theme notifications.
            write!(self.out(), "{}", decreset!(Theme))?;
        }

        Ok(())
    }

    // See <https://gist.github.com/christianparpart/d8a62cc1ab659194337d73e399004036>.
    // Synchronized output sequences tell the terminal when we are "starting to render" and
    // stopping, enabling to make better choices about when it draws a frame. This avoids all
    // kinds of ugly visual artifacts like tearing and flashing (i.e. the background color
    // after clearing the terminal).

    fn start_synchronized_render(&mut self) -> io::Result<()> {
        if self.capabilities.synchronized_output && !self.is_synchronized_output_set {
            write!(self.out(), "{}", decset!(SynchronizedOutput))?;
            self.is_synchronized_output_set = true;
        }
        Ok(())
    }

    fn end_sychronized_render(&mut self) -> io::Result<()> {
        if self.is_synchronized_output_set {
            write!(self.out(), "{}", decreset!(SynchronizedOutput))?;
            self.is_synchronized_output_set = false;
        }
        Ok(())
    }
}

impl Backend for TerminaBackend {
    fn claim(&mut self) -> io::Result<()> {
        self.terminal.enter_raw_mode()?;

        write!(
            self.out(),
            "{}{}{}{}",
            // Enter an alternate screen.
            decset!(ClearAndEnableAlternateScreen),
            decset!(BracketedPaste),
            decset!(FocusTracking),
            // Clear the buffer. `ClearAndEnableAlternateScreen` **should** do this but some
            // things like mosh are buggy. See <https://github.com/helix-editor/helix/pull/1944>.
            Csi::Edit(csi::Edit::EraseInDisplay(csi::EraseInDisplay::EraseDisplay)),
        )?;
        if let Some(color) = self.background_color {
            write!(
                self.out(),
                "{}",
                Osc::ChangeDynamicColors(
                    osc::DynamicColorNumber::TextBackgroundColor,
                    vec![color.into()]
                )
            )?;
        }
        self.enable_mouse_capture()?;
        self.enable_extensions()?;
        self.claimed = true;

        Ok(())
    }

    fn reconfigure(&mut self, mut config: Config) -> io::Result<()> {
        std::mem::swap(&mut self.config, &mut config);
        if self.claimed && self.config.enable_mouse_capture != config.enable_mouse_capture {
            if self.config.enable_mouse_capture {
                self.enable_mouse_capture()?;
            } else {
                self.disable_mouse_capture()?;
            }
        }
        self.capabilities.extended_underlines |= self.config.force_enable_extended_underlines;
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.claimed {
            return Ok(());
        }
        self.claimed = false;
        self.reset()?;
        self.out().flush()?;
        self.terminal.enter_cooked_mode()?;
        Ok(())
    }

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.start_synchronized_render()?;

        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut underline_color = Color::Reset;
        let mut underline_style = UnderlineStyle::Reset;
        let mut modifier = Modifier::empty();
        let mut last_pos: Option<(u16, u16)> = None;
        for (x, y, cell) in content {
            // Move the cursor if the previous location was not (x - 1, y)
            if !matches!(last_pos, Some(p) if x == p.0 + 1 && y == p.1) {
                write!(
                    self.out(),
                    "{}",
                    Csi::Cursor(csi::Cursor::Position {
                        col: OneBased::from_zero_based(x),
                        line: OneBased::from_zero_based(y),
                    })
                )?;
            }
            last_pos = Some((x, y));

            let mut attributes = SgrAttributes::default();
            if cell.fg != fg {
                attributes.foreground = Some(cell.fg.into());
                fg = cell.fg;
            }
            if cell.bg != bg {
                attributes.background = Some(cell.bg.into());
                bg = cell.bg;
            }
            if cell.modifier != modifier {
                attributes.modifiers = diff_modifiers(modifier, cell.modifier);
                modifier = cell.modifier;
            }

            // Set underline style and color separately from SgrAttributes. Some terminals seem
            // to not like underline colors and styles being intermixed with other SGRs.
            let mut new_underline_style = cell.underline_style;
            if self.capabilities.extended_underlines {
                if cell.underline_color != underline_color {
                    write!(
                        self.out(),
                        "{}",
                        Csi::Sgr(csi::Sgr::UnderlineColor(cell.underline_color.into()))
                    )?;
                    underline_color = cell.underline_color;
                }
            } else {
                match new_underline_style {
                    UnderlineStyle::Reset | UnderlineStyle::Line => (),
                    _ => new_underline_style = UnderlineStyle::Line,
                }
            }
            if new_underline_style != underline_style {
                write!(
                    self.out(),
                    "{}",
                    Csi::Sgr(csi::Sgr::Underline(new_underline_style.into()))
                )?;
                underline_style = new_underline_style;
            }

            // `attributes` will be empty if nothing changed between two cells. Empty
            // `SgrAttributes` behave the same as a `Sgr::Reset` rather than a 'no-op' though so
            // we should avoid writing them if they're empty.
            if !attributes.is_empty() {
                write!(self.out(), "{}", Csi::Sgr(csi::Sgr::Attributes(attributes)))?;
            }

            write!(self.out(), "{}", &cell.symbol)?;
        }

        write!(self.out(), "{}", Csi::Sgr(csi::Sgr::Reset))?;

        self.end_sychronized_render()?;

        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        write!(self.out(), "{}", decreset!(ShowCursor))?;
        self.flush()
    }

    fn show_cursor(&mut self, kind: CursorKind) -> io::Result<()> {
        let style = match kind {
            CursorKind::Block => CursorStyle::SteadyBlock,
            CursorKind::Bar => CursorStyle::SteadyBar,
            CursorKind::Underline => CursorStyle::SteadyUnderline,
            CursorKind::Hidden => unreachable!(),
        };
        write!(
            self.out(),
            "{}{}",
            decset!(ShowCursor),
            Csi::Cursor(csi::Cursor::CursorStyle(style)),
        )?;
        self.flush()
    }

    fn set_cursor(&mut self, x: u16, y: u16) -> io::Result<()> {
        let col = OneBased::from_zero_based(x);
        let line = OneBased::from_zero_based(y);
        write!(
            self.out(),
            "{}",
            Csi::Cursor(csi::Cursor::Position { line, col })
        )?;
        self.flush()
    }

    fn clear(&mut self) -> io::Result<()> {
        self.start_synchronized_render()?;
        write!(
            self.out(),
            "{}",
            Csi::Edit(csi::Edit::EraseInDisplay(csi::EraseInDisplay::EraseDisplay))
        )?;
        self.flush()
    }

    fn size(&self) -> io::Result<Rect> {
        let WindowSize { rows, cols, .. } = self.terminal.get_dimensions()?;
        Ok(Rect::new(0, 0, cols, rows))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out().flush()
    }

    fn supports_true_color(&self) -> bool {
        self.capabilities.true_color
    }

    fn get_theme_mode(&self) -> Option<theme::Mode> {
        self.capabilities.theme_mode
    }

    fn set_background_color(&mut self, color: Option<Color>) -> io::Result<()> {
        if !self.capabilities.dynamic_background_color {
            return Ok(());
        }
        self.background_color = match color {
            Some(Color::Rgb(r, g, b)) => Some(RgbColor::new(r, g, b)),
            _ => None,
        };
        if let Some(color) = self.background_color {
            write!(
                self.out(),
                "{}",
                Osc::ChangeDynamicColors(
                    osc::DynamicColorNumber::TextBackgroundColor,
                    vec![color.into()]
                )
            )
        } else {
            self.reset_background_color()
        }
    }
}

impl Drop for TerminaBackend {
    fn drop(&mut self) {
        // Avoid resetting the terminal while panicking because we set a panic hook above in
        // `Self::new`.
        if self.claimed && !std::thread::panicking() {
            let _ = self.reset();
            // NOTE: Drop for Platform terminal resets the mode and flushes the buffer when not
            // panicking.
        }
    }
}

fn diff_modifiers(from: Modifier, to: Modifier) -> SgrModifiers {
    let mut modifiers = SgrModifiers::default();

    let removed = from - to;
    if removed.contains(Modifier::REVERSED) {
        modifiers |= SgrModifiers::NO_REVERSE;
    }
    if removed.contains(Modifier::BOLD) {
        modifiers |= SgrModifiers::INTENSITY_NORMAL;
        if to.contains(Modifier::DIM) {
            modifiers |= SgrModifiers::INTENSITY_DIM
        }
    }
    if removed.contains(Modifier::DIM) {
        modifiers |= SgrModifiers::INTENSITY_NORMAL;
    }
    if removed.contains(Modifier::ITALIC) {
        modifiers |= SgrModifiers::NO_ITALIC;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        modifiers |= SgrModifiers::NO_STRIKE_THROUGH;
    }
    if removed.contains(Modifier::HIDDEN) {
        modifiers |= SgrModifiers::NO_INVISIBLE;
    }
    if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
        modifiers |= SgrModifiers::BLINK_NONE;
    }

    let added = to - from;
    if added.contains(Modifier::REVERSED) {
        modifiers |= SgrModifiers::REVERSE;
    }
    if added.contains(Modifier::BOLD) {
        modifiers |= SgrModifiers::INTENSITY_BOLD;
    }
    if added.contains(Modifier::DIM) {
        modifiers |= SgrModifiers::INTENSITY_DIM;
    }
    if added.contains(Modifier::ITALIC) {
        modifiers |= SgrModifiers::ITALIC;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        modifiers |= SgrModifiers::STRIKE_THROUGH;
    }
    if added.contains(Modifier::HIDDEN) {
        modifiers |= SgrModifiers::INVISIBLE;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        modifiers |= SgrModifiers::BLINK_SLOW;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        modifiers |= SgrModifiers::BLINK_RAPID;
    }

    modifiers
}

/// Writes to a terminal from a thread of its own, so a terminal that stops taking output, like
/// one over a stalled SSH connection, holds up only that thread. Writes are buffered until a
/// flush hands them to the thread.
#[derive(Debug)]
pub struct TerminalWriter {
    buffer: Vec<u8>,
    queue: mpsc::Sender<Queued>,
    /// How many bytes were handed to the thread but haven't been written yet.
    backlog: Arc<AtomicUsize>,
}

enum Queued {
    Bytes(Vec<u8>),
    Then(Box<dyn FnOnce() + Send>),
}

impl TerminalWriter {
    /// How far behind the terminal can be before [`Self::is_behind`]: about a full screen.
    const MAX_BACKLOG: usize = 64 * 1024;

    /// Starts writing to `terminal`. After the terminal has been behind, `caught_up` is called
    /// once it isn't anymore.
    pub fn new(mut terminal: File, caught_up: impl Fn() + Send + 'static) -> io::Result<Self> {
        let (queue, queued) = mpsc::channel();
        let backlog = Arc::new(AtomicUsize::new(0));
        let left = backlog.clone();
        std::thread::Builder::new()
            .name("terminal writer".into())
            .spawn(move || {
                let mut broken = false;
                let mut behind = false;
                for queued in queued {
                    match queued {
                        Queued::Bytes(bytes) => {
                            // Once the terminal is gone, what's left is dropped.
                            broken = broken || terminal.write_all(&bytes).is_err();
                            let before = left.fetch_sub(bytes.len(), Ordering::SeqCst);
                            behind |= before > Self::MAX_BACKLOG;
                            if behind && before - bytes.len() <= Self::MAX_BACKLOG {
                                behind = false;
                                caught_up();
                            }
                        }
                        Queued::Then(f) => f(),
                    }
                }
            })?;
        Ok(Self {
            buffer: Vec::new(),
            queue,
            backlog,
        })
    }

    fn is_behind(&self) -> bool {
        self.backlog.load(Ordering::SeqCst) > Self::MAX_BACKLOG
    }

    /// Runs `f` on the writer's thread once what was flushed before has been written.
    fn then(&self, f: impl FnOnce() + Send + 'static) {
        if let Err(mpsc::SendError(Queued::Then(f))) = self.queue.send(Queued::Then(Box::new(f))) {
            f();
        }
    }
}

/// Another handle on the same terminal and thread, with a buffer of its own.
impl Clone for TerminalWriter {
    fn clone(&self) -> Self {
        Self {
            buffer: Vec::new(),
            queue: self.queue.clone(),
            backlog: self.backlog.clone(),
        }
    }
}

impl Write for TerminalWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.buffer);
        self.backlog.fetch_add(bytes.len(), Ordering::SeqCst);
        self.queue
            .send(Queued::Bytes(bytes))
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

impl Drop for TerminalWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
