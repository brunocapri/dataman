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
            ResultState::Paused => s(crate::ui::PAUSE, "PAUSED", Role::Plain, "rows"),
            ResultState::Done => s("■", "DONE", Role::Ok, "rows"),
            ResultState::Affected => s("✓", "DONE", Role::Ok, "rows affected"),
            ResultState::Cancelling => s("◌", "CANCEL REQUESTED", Role::Warn, "rows kept"),
            ResultState::Cancelled => s("⊘", "CANCELLED", Role::Warn, "rows kept"),
            ResultState::Error => s("✕", "ERROR", Role::Err, ""),
            ResultState::ErrPartial => s("✕", "ERROR AFTER ROWS", Role::Err, "rows received"),
            ResultState::Text => s("≡", "TEXT PROTOCOL", Role::Plain, "rows"),
            ResultState::Sorting => s("↕", "SORTING", Role::Acc, "rows"),
            ResultState::Edit => s("✎", "PENDING", Role::Warn, "rows"),
            ResultState::Partial => s("◐", "PARTIAL", Role::Info, "rows kept"),
            ResultState::Messages => s("≡", "MESSAGES", Role::Plain, "statements"),
        }
    }
}

pub fn role_color(t: &Theme, r: Role) -> Hsla {
    match r {
        Role::Muted => t.tx3,
        Role::Plain => t.tx2,
        Role::Acc => t.acc,
        Role::Info => t.info,
        Role::Ok => t.ok,
        Role::Warn => t.warn,
        Role::Err => t.err,
    }
}
