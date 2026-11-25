//! The one logical work-desktop prompt (V2 spec §5.2): which reminder is
//! showing, and when a dismissed one comes back. Pure; `ui::tick` drives it
//! once a second and does the window calls.

/// A dismissed due/exercising/confirm prompt returns after this long.
pub const REPEAT_SECONDS: f64 = 5.0;

#[derive(Debug, Default)]
pub struct Prompt {
    pub kind: Option<&'static str>,
    pub visible: bool,
    hidden_at: Option<f64>,
}

#[derive(Debug, PartialEq)]
pub enum Act { Show, Hide, Keep }

impl Prompt {
    /// The reminder the session wants now (None = nothing owed). A new kind
    /// shows at once; the same kind is never re-shown while visible (no focus
    /// loop), and comes back REPEAT_SECONDS after a dismissal — except the
    /// start reminder, whose dismissal is final for that occurrence.
    pub fn update(&mut self, kind: Option<&'static str>, now: f64) -> Act {
        if kind != self.kind {
            self.kind = kind;
            self.hidden_at = None;
            let was = std::mem::replace(&mut self.visible, kind.is_some());
            return if kind.is_some() { Act::Show } else if was { Act::Hide } else { Act::Keep };
        }
        match (kind, self.visible, self.hidden_at) {
            (Some(k), false, Some(at)) if k != "start" && now - at >= REPEAT_SECONDS => {
                self.visible = true;
                self.hidden_at = None;
                Act::Show
            }
            _ => Act::Keep,
        }
    }

    /// Closed or "Not now": hidden until the repeat (if still owed).
    pub fn dismiss(&mut self, now: f64) {
        self.visible = false;
        self.hidden_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dismissed_due_prompt_returns_once_after_five_seconds() {
        let mut p = Prompt::default();
        assert_eq!(p.update(Some("due"), 0.0), Act::Show);
        assert_eq!(p.update(Some("due"), 1.0), Act::Keep); // visible: no re-show, no focus theft
        p.dismiss(10.0);
        assert_eq!(p.update(Some("due"), 14.0), Act::Keep);
        assert_eq!(p.update(Some("due"), 15.0), Act::Show);
        assert_eq!(p.update(Some("due"), 16.0), Act::Keep);
    }

    #[test]
    fn saving_or_snoozing_hides_every_copy_and_a_stale_repeat_cannot_reopen() {
        let mut p = Prompt::default();
        p.update(Some("confirm"), 0.0);
        p.dismiss(1.0);
        assert_eq!(p.update(None, 2.0), Act::Keep); // saved while hidden
        assert_eq!(p.update(None, 6.0), Act::Keep); // the old repeat is gone
        p.update(Some("due"), 7.0);
        assert_eq!(p.update(None, 8.0), Act::Hide);
    }

    #[test]
    fn start_reminder_dismissal_is_final_and_phase_changes_show_at_once() {
        let mut p = Prompt::default();
        p.update(Some("start"), 0.0);
        p.dismiss(1.0);
        assert_eq!(p.update(Some("start"), 100.0), Act::Keep);
        let mut p = Prompt::default();
        p.update(Some("due"), 0.0);
        p.dismiss(1.0);
        assert_eq!(p.update(Some("confirm"), 2.0), Act::Show); // target reached: ask for the weight now
    }
}
