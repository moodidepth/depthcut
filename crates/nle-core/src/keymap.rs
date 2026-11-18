//! Customisable keyboard shortcuts.
//!
//! Every editor command is an [`Action`]; an [`ActionMap`] binds actions to
//! one or more [`KeyBinding`]s (key + modifiers, or a mouse button). Maps are
//! stored as human-editable TOML:
//!
//! ```toml
//! profile = "Final Cut Pro"
//!
//! [bindings]
//! PlayPause = ["Space"]
//! Redo = ["Ctrl+Shift+Z"]
//! StepBack = ["Left", "Mouse:Back"]
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

macro_rules! actions {
    ($( $v:ident => $label:literal, $cat:literal; )*) => {
        /// Every bindable editor command.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Action { $($v),* }
        impl Action {
            pub const ALL: &'static [Action] = &[$(Action::$v),*];
            /// Stable identifier used in the TOML file.
            pub fn id(self) -> &'static str { match self { $(Action::$v => stringify!($v)),* } }
            pub fn label(self) -> &'static str { match self { $(Action::$v => $label),* } }
            pub fn category(self) -> &'static str { match self { $(Action::$v => $cat),* } }
            pub fn from_id(s: &str) -> Option<Action> { match s { $(stringify!($v) => Some(Action::$v),)* _ => None } }
        }
    };
}

actions! {
    PlayPause => "Play / Pause", "Playback";
    ShuttleLeft => "Shuttle Reverse 1x/2x/4x (J)", "Playback";
    ShuttleStop => "Pause / Hold for Frame Step (K)", "Playback";
    ShuttleRight => "Shuttle Forward 1x/2x/4x (L)", "Playback";
    StepBack => "Previous Frame", "Playback";
    StepForward => "Next Frame", "Playback";
    StepBack10 => "Back 10 Frames", "Playback";
    StepForward10 => "Forward 10 Frames", "Playback";
    PrevEdit => "Previous Edit Point", "Playback";
    NextEdit => "Next Edit Point", "Playback";
    GoToStart => "Go to Start", "Playback";
    GoToEnd => "Go to End", "Playback";
    SelectTool => "Select Tool", "Tools";
    TrimTool => "Trim Tool", "Tools";
    BladeTool => "Blade Tool", "Tools";
    RangeTool => "Range Tool", "Tools";
    PositionTool => "Position Tool (no ripple)", "Tools";
    BladeSplit => "Split Clip at Playhead", "Edit";
    BladeAll => "Blade All at Playhead", "Edit";
    RippleTrimStart => "Ripple Trim Start to Playhead (Q)", "Edit";
    RippleTrimEnd => "Ripple Trim End to Playhead (W)", "Edit";
    RippleDelete => "Ripple Delete", "Edit";
    DeleteWithGap => "Lift Delete (Leave Gap)", "Edit";
    InsertGap => "Insert Gap at Playhead", "Edit";
    Undo => "Undo", "Edit";
    Redo => "Redo", "Edit";
    SelectAll => "Select All", "Edit";
    ClearSelection => "Clear Selection", "Edit";
    ToggleClipEnabled => "Enable / Disable Clip", "Edit";
    AddCrossDissolve => "Add Cross Dissolve", "Edit";
    AddAdjustmentClip => "Add Adjustment Clip", "Edit";
    NewCompound => "New Compound Clip", "Edit";
    BreakApart => "Break Apart Clip Items", "Edit";
    NewStoryline => "Create Storyline", "Edit";
    StepOut => "Back Out of Compound / Storyline", "Edit";
    DuplicateAsAudition => "Duplicate as Audition", "Edit";
    AuditionNext => "Audition: Next Pick", "Edit";
    AuditionPrev => "Audition: Previous Pick", "Edit";
    FinalizeAudition => "Finalize Audition", "Edit";
    HoldFrame => "Hold Frame", "Edit";
    AppendClip => "Append Browser Clip", "Edit";
    InsertClip => "Insert Browser Clip", "Edit";
    ConnectClip => "Connect Browser Clip", "Edit";
    OverwriteClip => "Overwrite with Browser Clip", "Edit";
    AddKeyframe => "Add Keyframe", "Animation";
    ToggleAnimation => "Show Video Animation", "Animation";
    ToggleRetime => "Show Retime Editor", "Animation";
    AddMarker => "Add Marker", "Markers";
    AddNamedMarker => "Add Named / Colored Marker", "Markers";
    AnalyzeBeats => "Analyze Beats", "Markers";
    ScanKills => "Scan VOD for Kills / Action Peaks", "Montage";
    SyncToBeats => "Sync Clips & Velocity to Beats", "Montage";
    GamingReframe916 => "Gaming 9:16 Split-Crop", "Montage";
    ZoomPunch => "Zoom Punch at Playhead", "Montage";
    HardZoomPunch => "Hard Zoom Punch (hold)", "Montage";
    ToggleCopilot => "AI Editor Copilot", "AI";
    ToggleSnapping => "Snapping On/Off", "View";
    ToggleSkimming => "Skimming On/Off", "View";
    ZoomIn => "Zoom In", "View";
    ZoomOut => "Zoom Out", "View";
    ZoomToFit => "Zoom to Fit", "View";
    ToggleAudioLanes => "Show Audio Lanes by Role", "View";
    AutoCaptions => "Auto Captions", "Captions";
    Import => "Import Media", "File";
    ImportFcpxml => "Import FCPXML", "File";
    Open => "Open Project", "File";
    Save => "Save", "File";
    SaveAs => "Save As", "File";
    Export => "Export Movie", "File";
    ShowShortcuts => "Keyboard Shortcuts", "Help";
}

macro_rules! keys {
    ($( $v:ident => $name:literal ),* $(,)?) => {
        /// A physical key, independent of any UI toolkit.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum KeyCode { $($v),* }
        impl KeyCode {
            pub const ALL: &'static [KeyCode] = &[$(KeyCode::$v),*];
            pub fn name(self) -> &'static str { match self { $(KeyCode::$v => $name),* } }
            pub fn from_name(s: &str) -> Option<KeyCode> {
                $( if s.eq_ignore_ascii_case($name) { return Some(KeyCode::$v); } )*
                None
            }
        }
    };
}

keys! {
    A => "A", B => "B", C => "C", D => "D", E => "E", F => "F", G => "G", H => "H", I => "I", J => "J", K => "K", L => "L", M => "M",
    N => "N", O => "O", P => "P", Q => "Q", R => "R", S => "S", T => "T", U => "U", V => "V", W => "W", X => "X", Y => "Y", Z => "Z",
    Num0 => "0", Num1 => "1", Num2 => "2", Num3 => "3", Num4 => "4", Num5 => "5", Num6 => "6", Num7 => "7", Num8 => "8", Num9 => "9",
    F1 => "F1", F2 => "F2", F3 => "F3", F4 => "F4", F5 => "F5", F6 => "F6", F7 => "F7", F8 => "F8", F9 => "F9", F10 => "F10", F11 => "F11", F12 => "F12",
    Space => "Space", Enter => "Enter", Escape => "Esc", Tab => "Tab", Backspace => "Backspace", Delete => "Delete", Insert => "Insert",
    Home => "Home", End => "End", PageUp => "PageUp", PageDown => "PageDown",
    ArrowLeft => "Left", ArrowRight => "Right", ArrowUp => "Up", ArrowDown => "Down",
    Minus => "-", Equals => "=", Plus => "Plus", Comma => ",", Period => ".", Slash => "/", Backslash => "\\",
    OpenBracket => "[", CloseBracket => "]", Semicolon => ";", Quote => "'", Backtick => "`",
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MouseButton {
    Middle,
    /// Side button "back" (X1).
    Back,
    /// Side button "forward" (X2).
    Forward,
}

impl MouseButton {
    pub const ALL: [MouseButton; 3] = [MouseButton::Middle, MouseButton::Back, MouseButton::Forward];
    pub fn name(self) -> &'static str {
        match self {
            MouseButton::Middle => "Middle",
            MouseButton::Back => "Back",
            MouseButton::Forward => "Forward",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Modifiers {
    /// Ctrl on Windows/Linux (Cmd on macOS).
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers { ctrl: false, shift: false, alt: false };
}

/// A key (or mouse button) plus modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyBinding {
    pub mods: Modifiers,
    pub key: Option<KeyCode>,
    pub mouse: Option<MouseButton>,
}

impl KeyBinding {
    pub const fn key(key: KeyCode) -> Self {
        KeyBinding { mods: Modifiers::NONE, key: Some(key), mouse: None }
    }
    pub const fn ctrl(key: KeyCode) -> Self {
        KeyBinding { mods: Modifiers { ctrl: true, shift: false, alt: false }, key: Some(key), mouse: None }
    }
    pub const fn shift(key: KeyCode) -> Self {
        KeyBinding { mods: Modifiers { ctrl: false, shift: true, alt: false }, key: Some(key), mouse: None }
    }
    pub const fn alt(key: KeyCode) -> Self {
        KeyBinding { mods: Modifiers { ctrl: false, shift: false, alt: true }, key: Some(key), mouse: None }
    }
    pub const fn ctrl_shift(key: KeyCode) -> Self {
        KeyBinding { mods: Modifiers { ctrl: true, shift: true, alt: false }, key: Some(key), mouse: None }
    }
    pub const fn shift_alt(key: KeyCode) -> Self {
        KeyBinding { mods: Modifiers { ctrl: false, shift: true, alt: true }, key: Some(key), mouse: None }
    }
    pub const fn mouse(button: MouseButton) -> Self {
        KeyBinding { mods: Modifiers::NONE, key: None, mouse: Some(button) }
    }
    pub fn with_mods(mut self, mods: Modifiers) -> Self {
        self.mods = mods;
        self
    }
    pub fn is_valid(&self) -> bool {
        self.key.is_some() != self.mouse.is_some()
    }
}

impl fmt::Display for KeyBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<&str> = Vec::new();
        if self.mods.ctrl {
            parts.push("Ctrl");
        }
        if self.mods.shift {
            parts.push("Shift");
        }
        if self.mods.alt {
            parts.push("Alt");
        }
        let tail;
        match (self.key, self.mouse) {
            (Some(k), _) => parts.push(k.name()),
            (None, Some(m)) => {
                tail = format!("Mouse:{}", m.name());
                parts.push(&tail);
                return write!(f, "{}", parts.join("+"));
            }
            (None, None) => parts.push("?"),
        }
        write!(f, "{}", parts.join("+"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    #[error("unknown key in \"{0}\"")]
    BadBinding(String),
    #[error("unknown action \"{0}\"")]
    UnknownAction(String),
    #[error("TOML: {0}")]
    Toml(String),
    #[error("I/O: {0}")]
    Io(String),
}

impl FromStr for KeyBinding {
    type Err = KeymapError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || KeymapError::BadBinding(s.to_string());
        let mut mods = Modifiers::NONE;
        let mut key = None;
        let mut mouse = None;
        // split on '+' but keep a literal "+" / "Plus" key
        let tokens: Vec<&str> = if let Some(rest) = s.strip_suffix("++") {
            let mut t: Vec<&str> = rest.split('+').collect();
            t.push("Plus");
            t
        } else {
            s.split('+').collect()
        };
        for tok in tokens.iter().map(|t| t.trim()) {
            match tok.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "cmd" | "command" => mods.ctrl = true,
                "shift" => mods.shift = true,
                "alt" | "option" | "opt" => mods.alt = true,
                "" => return Err(bad()),
                other if other.starts_with("mouse:") => {
                    let name = &tok[6..];
                    mouse = Some(MouseButton::ALL.into_iter().find(|m| m.name().eq_ignore_ascii_case(name)).ok_or_else(bad)?);
                }
                _ => {
                    let k = KeyCode::from_name(tok)
                        .or_else(|| match tok.to_ascii_lowercase().as_str() {
                            "escape" => Some(KeyCode::Escape),
                            "del" => Some(KeyCode::Delete),
                            "return" => Some(KeyCode::Enter),
                            "arrowleft" => Some(KeyCode::ArrowLeft),
                            "arrowright" => Some(KeyCode::ArrowRight),
                            "arrowup" => Some(KeyCode::ArrowUp),
                            "arrowdown" => Some(KeyCode::ArrowDown),
                            "comma" => Some(KeyCode::Comma),
                            "minus" => Some(KeyCode::Minus),
                            "equals" => Some(KeyCode::Equals),
                            _ => None,
                        })
                        .ok_or_else(bad)?;
                    if key.is_some() {
                        return Err(bad());
                    }
                    key = Some(k);
                }
            }
        }
        let b = KeyBinding { mods, key, mouse };
        if b.is_valid() {
            Ok(b)
        } else {
            Err(bad())
        }
    }
}

pub const PROFILE_ARGON: &str = "Argon Montage";
pub const PROFILE_FCP: &str = "Final Cut Pro";
pub const PROFILE_RESOLVE: &str = "DaVinci Resolve";
pub const PROFILES: [&str; 3] = [PROFILE_ARGON, PROFILE_FCP, PROFILE_RESOLVE];

/// Action → bindings table.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionMap {
    pub profile: String,
    pub bindings: BTreeMap<Action, Vec<KeyBinding>>,
}

#[derive(Serialize, Deserialize)]
struct KeymapFile {
    profile: String,
    #[serde(default)]
    bindings: BTreeMap<String, Vec<String>>,
}

impl Default for ActionMap {
    fn default() -> Self {
        ActionMap::argon_montage()
    }
}

impl ActionMap {
    fn from_pairs(profile: &str, pairs: &[(Action, &[KeyBinding])]) -> Self {
        let mut bindings: BTreeMap<Action, Vec<KeyBinding>> = Action::ALL.iter().map(|a| (*a, Vec::new())).collect();
        for (a, b) in pairs {
            bindings.insert(*a, b.to_vec());
        }
        ActionMap { profile: profile.to_string(), bindings }
    }

    /// Argon Montage speed-cutting defaults (`Q`/`W` ripple trims, `C`/`Ctrl+B` split,
    /// `S` snapping, `Delete` lift / `Shift+Delete` ripple, `M`/`Alt+M` markers).
    pub fn argon_montage() -> Self {
        use Action::*;
        use KeyBinding as B;
        use KeyCode as K;
        Self::from_pairs(
            PROFILE_ARGON,
            &[
                (PlayPause, &[B::key(K::Space)]),
                (ShuttleLeft, &[B::key(K::J)]),
                (ShuttleStop, &[B::key(K::K)]),
                (ShuttleRight, &[B::key(K::L)]),
                (StepBack, &[B::key(K::ArrowLeft)]),
                (StepForward, &[B::key(K::ArrowRight)]),
                (StepBack10, &[B::shift(K::ArrowLeft)]),
                (StepForward10, &[B::shift(K::ArrowRight)]),
                (PrevEdit, &[B::key(K::ArrowUp)]),
                (NextEdit, &[B::key(K::ArrowDown)]),
                (GoToStart, &[B::key(K::Home)]),
                (GoToEnd, &[B::key(K::End)]),
                (SelectTool, &[B::key(K::A)]),
                (TrimTool, &[B::key(K::T)]),
                (BladeTool, &[B::ctrl_shift(K::B)]),
                (RangeTool, &[B::key(K::R)]),
                (BladeSplit, &[B::key(K::C), B::ctrl(K::B), B::key(K::B)]),
                (BladeAll, &[B::shift(K::B)]),
                (RippleTrimStart, &[B::key(K::Q)]),
                (RippleTrimEnd, &[B::key(K::W)]),
                (RippleDelete, &[B::shift(K::Delete), B::shift(K::Backspace)]),
                (DeleteWithGap, &[B::key(K::Delete), B::key(K::Backspace)]),
                (InsertGap, &[B::alt(K::G)]),
                (Undo, &[B::ctrl(K::Z)]),
                (Redo, &[B::ctrl_shift(K::Z)]),
                (SelectAll, &[B::ctrl(K::A)]),
                (ClearSelection, &[B::key(K::Escape)]),
                (ToggleClipEnabled, &[B::key(K::V)]),
                (AddCrossDissolve, &[B::ctrl(K::T)]),
                (AddAdjustmentClip, &[B::alt(K::A)]),
                (NewCompound, &[B::alt(K::C)]),
                (BreakApart, &[B::shift_alt(K::C)]),
                (HoldFrame, &[B::shift(K::H)]),
                (AppendClip, &[B::key(K::E)]),
                (InsertClip, &[B::alt(K::W)]),
                (ConnectClip, &[B::alt(K::Q)]),
                (OverwriteClip, &[B::key(K::D)]),
                (AddKeyframe, &[B::alt(K::K)]),
                (ToggleAnimation, &[B::ctrl(K::V)]),
                (ToggleRetime, &[B::ctrl(K::R)]),
                (AddMarker, &[B::key(K::M)]),
                (AddNamedMarker, &[B::alt(K::M)]),
                (AnalyzeBeats, &[B::alt(K::B)]),
                (ScanKills, &[B::shift_alt(K::K)]),
                (SyncToBeats, &[B::alt(K::S)]),
                (GamingReframe916, &[B::alt(K::Num9)]),
                (ZoomPunch, &[B::key(K::Z)]),
                (HardZoomPunch, &[B::shift(K::Z)]),
                (ToggleCopilot, &[B::ctrl_shift(K::A)]),
                (ToggleSnapping, &[B::key(K::S), B::key(K::N)]),
                (ToggleSkimming, &[B::shift(K::S)]),
                (ZoomIn, &[B::ctrl(K::Equals), B::ctrl(K::Plus)]),
                (ZoomOut, &[B::ctrl(K::Minus)]),
                (ZoomToFit, &[B::ctrl(K::Num0)]),
                (AutoCaptions, &[B::ctrl_shift(K::C)]),
                (Import, &[B::ctrl(K::I)]),
                (ImportFcpxml, &[B::ctrl_shift(K::I)]),
                (Open, &[B::ctrl(K::O)]),
                (Save, &[B::ctrl(K::S)]),
                (SaveAs, &[B::ctrl_shift(K::S)]),
                (Export, &[B::ctrl(K::E)]),
                (ShowShortcuts, &[B::ctrl(K::Comma), B::key(K::F1)]),
                (PositionTool, &[B::key(K::P)]),
                (NewStoryline, &[B::ctrl(K::G)]),
                (StepOut, &[B::ctrl(K::OpenBracket)]),
                (DuplicateAsAudition, &[B::ctrl_shift(K::Y)]),
                (AuditionNext, &[B::ctrl(K::ArrowRight)]),
                (AuditionPrev, &[B::ctrl(K::ArrowLeft)]),
                (FinalizeAudition, &[B::shift_alt(K::Y)]),
                (ToggleAudioLanes, &[B::ctrl_shift(K::L)]),
            ],
        )
    }

    /// Final Cut Pro defaults (Ctrl replaces ⌘).
    pub fn final_cut_pro() -> Self {
        use Action::*;
        use KeyBinding as B;
        use KeyCode as K;
        Self::from_pairs(
            PROFILE_FCP,
            &[
                (PlayPause, &[B::key(K::Space)]),
                (ShuttleLeft, &[B::key(K::J)]),
                (ShuttleStop, &[B::key(K::K)]),
                (ShuttleRight, &[B::key(K::L)]),
                (StepBack, &[B::key(K::ArrowLeft)]),
                (StepForward, &[B::key(K::ArrowRight)]),
                (StepBack10, &[B::shift(K::ArrowLeft)]),
                (StepForward10, &[B::shift(K::ArrowRight)]),
                (PrevEdit, &[B::key(K::ArrowUp)]),
                (NextEdit, &[B::key(K::ArrowDown)]),
                (GoToStart, &[B::key(K::Home)]),
                (GoToEnd, &[B::key(K::End)]),
                (SelectTool, &[B::key(K::A)]),
                (TrimTool, &[B::key(K::T)]),
                (BladeTool, &[B::key(K::B)]),
                (RangeTool, &[B::key(K::R)]),
                (BladeSplit, &[B::ctrl(K::B)]),
                (BladeAll, &[B::ctrl_shift(K::B)]),
                (RippleTrimStart, &[B::alt(K::OpenBracket)]),
                (RippleTrimEnd, &[B::alt(K::CloseBracket)]),
                (RippleDelete, &[B::key(K::Delete), B::key(K::Backspace)]),
                (DeleteWithGap, &[B::shift(K::Delete), B::shift(K::Backspace)]),
                (InsertGap, &[B::alt(K::W)]),
                (Undo, &[B::ctrl(K::Z)]),
                (Redo, &[B::ctrl_shift(K::Z)]),
                (SelectAll, &[B::ctrl(K::A)]),
                (ClearSelection, &[B::key(K::Escape)]),
                (ToggleClipEnabled, &[B::key(K::V)]),
                (AddCrossDissolve, &[B::ctrl(K::T)]),
                (AddAdjustmentClip, &[B::alt(K::A)]),
                (NewCompound, &[B::alt(K::G)]),
                (BreakApart, &[B::shift_alt(K::G)]),
                (HoldFrame, &[B::shift(K::H)]),
                (AppendClip, &[B::key(K::E)]),
                (InsertClip, &[B::key(K::W)]),
                (ConnectClip, &[B::key(K::Q)]),
                (OverwriteClip, &[B::key(K::D)]),
                (AddKeyframe, &[B::alt(K::K)]),
                (ToggleAnimation, &[B::ctrl(K::V)]),
                (ToggleRetime, &[B::ctrl(K::R)]),
                (AddMarker, &[B::key(K::M)]),
                (AddNamedMarker, &[B::alt(K::M)]),
                (AnalyzeBeats, &[B::alt(K::B)]),
                (ScanKills, &[B::shift_alt(K::K)]),
                (SyncToBeats, &[B::alt(K::S)]),
                (GamingReframe916, &[B::alt(K::Num9)]),
                (ZoomPunch, &[B::key(K::Z)]),
                (HardZoomPunch, &[B::shift(K::Z)]),
                (ToggleCopilot, &[B::ctrl_shift(K::A)]),
                (ToggleSnapping, &[B::key(K::N)]),
                (ToggleSkimming, &[B::key(K::S)]),
                (ZoomIn, &[B::ctrl(K::Equals), B::ctrl(K::Plus)]),
                (ZoomOut, &[B::ctrl(K::Minus)]),
                (ZoomToFit, &[B::ctrl(K::Num0)]),
                (AutoCaptions, &[B::ctrl_shift(K::C)]),
                (Import, &[B::ctrl(K::I)]),
                (ImportFcpxml, &[B::ctrl_shift(K::I)]),
                (Open, &[B::ctrl(K::O)]),
                (Save, &[B::ctrl(K::S)]),
                (SaveAs, &[B::ctrl_shift(K::S)]),
                (Export, &[B::ctrl(K::E)]),
                (ShowShortcuts, &[B::ctrl(K::Comma), B::key(K::F1)]),
                (PositionTool, &[B::key(K::P)]),
                (NewStoryline, &[B::ctrl(K::G)]),
                (StepOut, &[B::ctrl(K::OpenBracket)]),
                (DuplicateAsAudition, &[B::ctrl_shift(K::Y)]),
                (AuditionNext, &[B::ctrl(K::ArrowRight)]),
                (AuditionPrev, &[B::ctrl(K::ArrowLeft)]),
                (FinalizeAudition, &[B::shift_alt(K::Y)]),
                (ToggleAudioLanes, &[B::ctrl_shift(K::L)]),
            ],
        )
    }

    /// DaVinci Resolve Edit-page style defaults.
    pub fn davinci_resolve() -> Self {
        use Action::*;
        use KeyBinding as B;
        use KeyCode as K;
        Self::from_pairs(
            PROFILE_RESOLVE,
            &[
                (PlayPause, &[B::key(K::Space)]),
                (ShuttleLeft, &[B::key(K::J)]),
                (ShuttleStop, &[B::key(K::K)]),
                (ShuttleRight, &[B::key(K::L)]),
                (StepBack, &[B::key(K::ArrowLeft)]),
                (StepForward, &[B::key(K::ArrowRight)]),
                (StepBack10, &[B::shift(K::ArrowLeft)]),
                (StepForward10, &[B::shift(K::ArrowRight)]),
                (PrevEdit, &[B::key(K::ArrowUp)]),
                (NextEdit, &[B::key(K::ArrowDown)]),
                (GoToStart, &[B::key(K::Home)]),
                (GoToEnd, &[B::key(K::End)]),
                (SelectTool, &[B::key(K::A)]),
                (TrimTool, &[B::key(K::T)]),
                (BladeTool, &[B::key(K::B)]),
                (RangeTool, &[B::key(K::R)]),
                (BladeSplit, &[B::ctrl(K::B), B::ctrl(K::Backslash)]),
                (BladeAll, &[B::ctrl_shift(K::B)]),
                (RippleTrimStart, &[B::alt(K::OpenBracket)]),
                (RippleTrimEnd, &[B::alt(K::CloseBracket)]),
                (RippleDelete, &[B::key(K::Delete), B::shift(K::Backspace)]),
                (DeleteWithGap, &[B::key(K::Backspace)]),
                (InsertGap, &[B::alt(K::W)]),
                (Undo, &[B::ctrl(K::Z)]),
                (Redo, &[B::ctrl_shift(K::Z)]),
                (SelectAll, &[B::ctrl(K::A)]),
                (ClearSelection, &[B::key(K::Escape)]),
                (ToggleClipEnabled, &[B::key(K::D)]),
                (AddCrossDissolve, &[B::ctrl(K::T)]),
                (AddAdjustmentClip, &[B::alt(K::A)]),
                (NewCompound, &[B::alt(K::G)]),
                (BreakApart, &[B::shift_alt(K::G)]),
                (HoldFrame, &[B::shift(K::R)]),
                (AppendClip, &[B::shift(K::F12)]),
                (InsertClip, &[B::key(K::F9)]),
                (ConnectClip, &[B::key(K::F12)]),
                (OverwriteClip, &[B::key(K::F10)]),
                (AddKeyframe, &[B::alt(K::K)]),
                (ToggleAnimation, &[B::shift(K::C)]),
                (ToggleRetime, &[B::ctrl(K::R)]),
                (AddMarker, &[B::key(K::M)]),
                (AddNamedMarker, &[B::alt(K::M)]),
                (AnalyzeBeats, &[B::alt(K::B)]),
                (ScanKills, &[B::shift_alt(K::K)]),
                (SyncToBeats, &[B::alt(K::S)]),
                (GamingReframe916, &[B::alt(K::Num9)]),
                (ZoomPunch, &[B::key(K::Z)]),
                (HardZoomPunch, &[B::shift(K::Z)]),
                (ToggleCopilot, &[B::ctrl_shift(K::A)]),
                (ToggleSnapping, &[B::key(K::N)]),
                (ToggleSkimming, &[B::key(K::S)]),
                (ZoomIn, &[B::ctrl(K::Equals), B::ctrl(K::Plus)]),
                (ZoomOut, &[B::ctrl(K::Minus)]),
                (ZoomToFit, &[B::ctrl(K::Num0)]),
                (AutoCaptions, &[B::ctrl_shift(K::C)]),
                (Import, &[B::ctrl(K::I)]),
                (ImportFcpxml, &[B::ctrl_shift(K::I)]),
                (Open, &[B::ctrl(K::O)]),
                (Save, &[B::ctrl(K::S)]),
                (SaveAs, &[B::ctrl_shift(K::S)]),
                (Export, &[B::ctrl(K::E)]),
                (ShowShortcuts, &[B::ctrl(K::Comma), B::key(K::F1)]),
                (PositionTool, &[B::key(K::P)]),
                (NewStoryline, &[B::ctrl(K::G)]),
                (StepOut, &[B::ctrl(K::OpenBracket)]),
                (DuplicateAsAudition, &[B::ctrl_shift(K::Y)]),
                (AuditionNext, &[B::ctrl(K::ArrowRight)]),
                (AuditionPrev, &[B::ctrl(K::ArrowLeft)]),
                (FinalizeAudition, &[B::shift_alt(K::Y)]),
                (ToggleAudioLanes, &[B::ctrl_shift(K::L)]),
            ],
        )
    }

    pub fn preset(name: &str) -> Option<Self> {
        match name {
            PROFILE_ARGON => Some(Self::argon_montage()),
            PROFILE_FCP => Some(Self::final_cut_pro()),
            PROFILE_RESOLVE => Some(Self::davinci_resolve()),
            _ => None,
        }
    }

    pub fn get(&self, a: Action) -> &[KeyBinding] {
        self.bindings.get(&a).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Replace all bindings of `a`.
    pub fn set(&mut self, a: Action, bindings: Vec<KeyBinding>) {
        self.bindings.insert(a, bindings);
    }

    pub fn add(&mut self, a: Action, b: KeyBinding) {
        let v = self.bindings.entry(a).or_default();
        if !v.contains(&b) {
            v.push(b);
        }
    }

    pub fn remove(&mut self, a: Action, b: KeyBinding) {
        if let Some(v) = self.bindings.get_mut(&a) {
            v.retain(|x| *x != b);
        }
    }

    /// Every action bound to exactly `b`.
    pub fn actions_for(&self, b: &KeyBinding) -> Vec<Action> {
        self.bindings.iter().filter(|(_, v)| v.contains(b)).map(|(a, _)| *a).collect()
    }

    /// The first action bound to `b`.
    pub fn lookup(&self, b: &KeyBinding) -> Option<Action> {
        self.bindings.iter().find(|(_, v)| v.contains(b)).map(|(a, _)| *a)
    }

    /// Bindings used by more than one action.
    pub fn conflicts(&self) -> Vec<(KeyBinding, Vec<Action>)> {
        let mut by: BTreeMap<KeyBinding, Vec<Action>> = BTreeMap::new();
        for (a, v) in &self.bindings {
            for b in v {
                by.entry(*b).or_default().push(*a);
            }
        }
        by.into_iter().filter(|(_, a)| a.len() > 1).collect()
    }

    /// Human readable label of the first binding (for menus).
    pub fn hint(&self, a: Action) -> String {
        self.get(a).first().map(|b| b.to_string()).unwrap_or_default()
    }

    pub fn to_toml(&self) -> String {
        let file =
            KeymapFile { profile: self.profile.clone(), bindings: self.bindings.iter().map(|(a, v)| (a.id().to_string(), v.iter().map(|b| b.to_string()).collect())).collect() };
        toml::to_string_pretty(&file).unwrap_or_default()
    }

    /// Parse TOML. Actions missing from the file keep the profile's defaults,
    /// so new commands get a binding after an update.
    pub fn from_toml(s: &str) -> Result<Self, KeymapError> {
        let file: KeymapFile = toml::from_str(s).map_err(|e| KeymapError::Toml(e.to_string()))?;
        let mut map = ActionMap::preset(&file.profile).unwrap_or_default();
        map.profile = file.profile;
        for (id, list) in file.bindings {
            let a = Action::from_id(&id).ok_or(KeymapError::UnknownAction(id))?;
            let v = list.iter().map(|s| s.parse()).collect::<Result<Vec<KeyBinding>, _>>()?;
            map.set(a, v);
        }
        Ok(map)
    }

    pub fn load(path: &Path) -> Result<Self, KeymapError> {
        let s = std::fs::read_to_string(path).map_err(|e| KeymapError::Io(e.to_string()))?;
        Self::from_toml(&s)
    }

    pub fn save(&self, path: &Path) -> Result<(), KeymapError> {
        crate::autosave::write_atomic(path, self.to_toml().as_bytes()).map_err(|e| KeymapError::Io(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_bind_core_actions_without_conflicts() {
        for name in PROFILES {
            let m = ActionMap::preset(name).unwrap();
            assert!(m.conflicts().is_empty(), "{name}: {:?}", m.conflicts());
            for a in [Action::PlayPause, Action::BladeSplit, Action::Undo, Action::Redo, Action::Export, Action::ZoomIn, Action::ShowShortcuts] {
                assert!(!m.get(a).is_empty(), "{name}: {a:?} unbound");
            }
            assert!(m.bindings.len() == Action::ALL.len());
        }
        let fcp = ActionMap::final_cut_pro();
        assert_eq!(fcp.lookup(&KeyBinding::ctrl(KeyCode::B)), Some(Action::BladeSplit));
        assert_eq!(fcp.lookup(&KeyBinding::key(KeyCode::Space)), Some(Action::PlayPause));
        let dv = ActionMap::davinci_resolve();
        assert_eq!(dv.lookup(&KeyBinding::key(KeyCode::Backspace)), Some(Action::DeleteWithGap));
        assert_eq!(fcp.lookup(&KeyBinding::key(KeyCode::Backspace)), Some(Action::RippleDelete));
    }

    #[test]
    fn binding_strings_round_trip() {
        for s in ["Ctrl+Shift+Z", "Space", "Alt+K", "Shift+Left", "Ctrl+=", "Ctrl+Plus", "Mouse:Back", "Ctrl+Mouse:Middle", "F12", "Ctrl+,"] {
            let b: KeyBinding = s.parse().unwrap_or_else(|e| panic!("{s}: {e}"));
            assert_eq!(b.to_string().parse::<KeyBinding>().unwrap(), b, "{s}");
        }
        assert_eq!("ctrl+shift+z".parse::<KeyBinding>().unwrap(), KeyBinding::ctrl_shift(KeyCode::Z));
        assert_eq!("Ctrl++".parse::<KeyBinding>().unwrap(), KeyBinding::ctrl(KeyCode::Plus));
        assert_eq!("Escape".parse::<KeyBinding>().unwrap(), KeyBinding::key(KeyCode::Escape));
        for bad in ["", "Ctrl+", "Ctrl+Foo", "A+B", "Mouse:Nope", "Ctrl+Shift"] {
            assert!(bad.parse::<KeyBinding>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn conflicts_are_detected() {
        let mut m = ActionMap::final_cut_pro();
        m.add(Action::Export, KeyBinding::key(KeyCode::Space));
        let c = m.conflicts();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, KeyBinding::key(KeyCode::Space));
        assert_eq!(c[0].1, vec![Action::PlayPause, Action::Export]);
        m.remove(Action::Export, KeyBinding::key(KeyCode::Space));
        assert!(m.conflicts().is_empty());
    }

    #[test]
    fn toml_round_trip_and_defaults_for_missing_actions() {
        let mut m = ActionMap::davinci_resolve();
        m.set(Action::PlayPause, vec![KeyBinding::key(KeyCode::P), KeyBinding::mouse(MouseButton::Middle)]);
        let s = m.to_toml();
        assert!(s.contains("profile = \"DaVinci Resolve\""), "{s}");
        assert!(s.contains("PlayPause = ["), "{s}");
        let back = ActionMap::from_toml(&s).unwrap();
        assert_eq!(back, m);

        let partial = "profile = \"Final Cut Pro\"\n[bindings]\nUndo = [\"Ctrl+U\"]\n";
        let p = ActionMap::from_toml(partial).unwrap();
        assert_eq!(p.get(Action::Undo), &[KeyBinding::ctrl(KeyCode::U)]);
        assert_eq!(p.get(Action::Redo), &[KeyBinding::ctrl_shift(KeyCode::Z)]);
        assert!(ActionMap::from_toml("profile = \"x\"\n[bindings]\nNope = [\"A\"]\n").is_err());
        assert!(ActionMap::from_toml("profile = \"x\"\n[bindings]\nUndo = [\"Hyper+Q\"]\n").is_err());
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join(format!("rcp-keymap-{}", std::process::id()));
        let path = dir.join("config").join("keybindings.toml");
        let m = ActionMap::final_cut_pro();
        m.save(&path).unwrap();
        assert_eq!(ActionMap::load(&path).unwrap(), m);
        let _ = std::fs::remove_dir_all(dir);
    }
}
