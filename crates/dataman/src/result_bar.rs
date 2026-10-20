use gpui::Hsla;

use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultState {
    Idle,
    Running,
    Paused,
    Done,
    Affected,
    Cancelling,
    Cancelled,
    Error,
    ErrPartial,
    Text,
    Sorting,
    Live,
    Edit,
    Partial,
    Messages,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Muted,
    Plain,
    Acc,
    Info,
    Ok,
    Warn,
    Err,
    Lar,
}

pub struct Spec {
    pub glyph: &'static str,
    pub label: &'static str,
    pub role: Role,
    pub rows_label: &'static str,
}

impl ResultState {
    pub fn spec(self) -> Spec {
        let s = |glyph, label, role, rows_label| Spec { glyph, label, role, rows_label };
        match self {
            ResultState::Idle => s("○", "NO RESULT", Role::Muted, ""),
            ResultState::Running => s("▶", "RUNNING", Role::Acc, "rows so far"),
            ResultState::Paused => s("‖", "PAUSED", Role::Info, "rows"),
            ResultState::Done => s("■", "DONE", Role::Ok, "rows"),
            ResultState::Affected => s("✓", "DONE", Role::Ok, "rows affected"),
            ResultState::Cancelling => s("◌", "CANCEL REQUESTED", Role::Warn, "rows kept"),
            ResultState::Cancelled => s("⊘", "CANCELLED", Role::Warn, "rows kept"),
            ResultState::Error => s("✕", "ERROR", Role::Err, ""),
            ResultState::ErrPartial => s("✕", "ERROR AFTER ROWS", Role::Err, "rows received"),
            ResultState::Text => s("≡", "TEXT PROTOCOL", Role::Plain, "rows"),
            ResultState::Sorting => s("↕", "SORTING", Role::Acc, "rows"),
            ResultState::Live => s("●", "LIVE", Role::Lar, "rows"),
            ResultState::Edit => s("✎", "PENDING", Role::Warn, "rows"),
            ResultState::Partial => s("◐", "PARTIAL", Role::Info, "rows kept"),
            ResultState::Messages => s("≡", "MESSAGES", Role::Plain, "statements"),
        }
    }
}

pub fn role_colors(t: &Theme, r: Role) -> (Hsla, Hsla) {
    match r {
        Role::Muted => (t.tx3, t.hov),
        Role::Plain => (t.tx2, t.hov),
        Role::Acc => (t.acc, t.acc_s),
        Role::Info => (t.info, t.info_s),
        Role::Ok => (t.ok, t.ok_s),
        Role::Warn => (t.warn, t.warn_s),
        Role::Err => (t.err, t.err_s),
        Role::Lar => (t.lar, t.lar_s),
    }
}
