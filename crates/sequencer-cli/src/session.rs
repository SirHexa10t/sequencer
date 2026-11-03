//! Which program has focus, and which keyboard layout is active.
//!
//! What the `program` and `kb_lang` gates match on, and what `detect-key` prints as its
//! `focus:` and `lang:` lines. One module decides who is asked, so the gates and the lines
//! that teach their spelling can never consult different sources:
//!
//! - On X11, the X server: EWMH for focus, XKB for the layout. It sees every window.
//! - In a Wayland session, KDE where it runs — KWin for focus, its keyboard service for the
//!   layout — because the X server there is Xwayland, which sees only X11 windows and names
//!   only the first layout. Without KDE, Xwayland is still asked: X11 windows answer.

#[cfg(all(feature = "xtest", target_os = "linux"))]
use sequencer_input::{FocusWatcher, LayoutWatcher};
#[cfg(target_os = "linux")]
use sequencer_input::{KWinFocus, KdeLayout};

/// Who answers a question about the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// A plain X server, which sees every window.
    X11,
    /// Xwayland, which sees only X11 windows.
    Xwayland,
    /// KDE: KWin for focus, its keyboard service for the layout.
    Kde,
}

/// The sources to try, best first: the X server on X11, KDE in a Wayland session (and
/// wherever there is no X server), then Xwayland's partial view.
fn candidates() -> &'static [Source] {
    if wayland_session() {
        &[Source::Kde, Source::Xwayland]
    } else {
        &[Source::X11, Source::Kde]
    }
}

/// Whether this is a Wayland session: `$WAYLAND_DISPLAY` says so, or — when sudo dropped
/// that variable — the X server turns out to be Xwayland.
pub(crate) fn wayland_session() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return true;
    }
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    {
        sequencer_input::x11::is_xwayland()
    }
    #[cfg(not(all(feature = "xtest", target_os = "linux")))]
    {
        false
    }
}

/// A focused window, for telling "still the window this run began in" from "another one".
/// Compared, never shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Window {
    /// An X window id.
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    X11(u32),
    /// KWin's id for a window.
    #[cfg(target_os = "linux")]
    KWin(String),
}

/// Answers which program has focus.
pub(crate) enum Focus {
    /// The X server, or Xwayland, through EWMH. Boxed: the X connection is most of a
    /// kilobyte, the other sources a few words.
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    X11(Box<FocusWatcher>),
    /// KWin, through a script it runs.
    #[cfg(target_os = "linux")]
    KWin(KWinFocus),
}

impl Focus {
    /// The best source that answers here, or `None` when nothing can.
    pub(crate) fn open() -> Option<Self> {
        candidates()
            .iter()
            .find_map(|&source| Self::open_source(source))
    }

    /// Who [`Self::open`] would ask, found without loading anything into KWin.
    pub(crate) fn source() -> Option<Source> {
        candidates().iter().copied().find(|&source| match source {
            #[cfg(target_os = "linux")]
            Source::Kde => KWinFocus::available(),
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            Source::X11 | Source::Xwayland => FocusWatcher::open().is_some(),
            #[cfg(not(all(feature = "xtest", target_os = "linux")))]
            _ => false,
        })
    }

    fn open_source(source: Source) -> Option<Self> {
        match source {
            #[cfg(target_os = "linux")]
            Source::Kde => KWinFocus::open().map(Self::KWin),
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            Source::X11 | Source::Xwayland => {
                FocusWatcher::open().map(|watcher| Self::X11(Box::new(watcher)))
            }
            #[cfg(not(all(feature = "xtest", target_os = "linux")))]
            _ => None,
        }
    }

    /// The focused program's name, as a `program` pattern matches it.
    pub(crate) fn program(&self) -> Option<String> {
        match *self {
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            Self::X11(ref watcher) => watcher.focused_class(),
            #[cfg(target_os = "linux")]
            Self::KWin(ref kwin) => kwin.focused_class(),
        }
    }

    /// The focused window.
    pub(crate) fn window(&self) -> Option<Window> {
        match *self {
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            Self::X11(ref watcher) => watcher.focused_window().map(Window::X11),
            #[cfg(target_os = "linux")]
            Self::KWin(ref kwin) => kwin.focused_window().map(Window::KWin),
        }
    }
}

/// Answers which keyboard layout is active.
pub(crate) enum Layout {
    /// The X server, or Xwayland, through XKB. Boxed, like [`Focus::X11`].
    #[cfg(all(feature = "xtest", target_os = "linux"))]
    X11(Box<LayoutWatcher>),
    /// KDE's keyboard service.
    #[cfg(target_os = "linux")]
    Kde(KdeLayout),
}

impl Layout {
    /// The best source that answers here, or `None` when nothing can.
    pub(crate) fn open() -> Option<Self> {
        candidates()
            .iter()
            .find_map(|&source| Self::open_source(source))
    }

    /// Who [`Self::open`] would ask.
    pub(crate) fn source() -> Option<Source> {
        candidates().iter().copied().find(|&source| match source {
            #[cfg(target_os = "linux")]
            Source::Kde => KdeLayout::available(),
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            Source::X11 | Source::Xwayland => LayoutWatcher::open().is_some(),
            #[cfg(not(all(feature = "xtest", target_os = "linux")))]
            _ => false,
        })
    }

    fn open_source(source: Source) -> Option<Self> {
        match source {
            #[cfg(target_os = "linux")]
            Source::Kde => KdeLayout::open().map(Self::Kde),
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            Source::X11 | Source::Xwayland => {
                LayoutWatcher::open().map(|watcher| Self::X11(Box::new(watcher)))
            }
            #[cfg(not(all(feature = "xtest", target_os = "linux")))]
            _ => None,
        }
    }

    /// The active layout's own name (`us`, `il`), as a `kb_lang` pattern matches it.
    pub(crate) fn current(&self) -> Option<String> {
        match *self {
            #[cfg(all(feature = "xtest", target_os = "linux"))]
            Self::X11(ref watcher) => watcher.current(),
            #[cfg(target_os = "linux")]
            Self::Kde(ref kde) => kde.current(),
        }
    }
}
