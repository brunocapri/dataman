//! Modals fade in as they open; the ones that are views also fade out as they close.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{AnyElement, AnyView, InteractiveElement, IntoElement, ParentElement, Styled, Window, div};

use crate::motion::EASE;

const IN: Duration = Duration::from_millis(160);
const OUT: Duration = Duration::from_millis(120);

#[derive(Default)]
pub(crate) struct Fades {
    opened: HashMap<&'static str, Instant>,
    views: HashMap<&'static str, AnyView>,
    closing: HashMap<&'static str, (AnyView, Instant)>,
}

fn layer(el: AnyElement, k: f32) -> gpui::Div {
    div().absolute().top_0().left_0().right_0().bottom_0().opacity(EASE.eval(k)).child(el)
}

impl Fades {
    fn coming_in(&mut self, key: &'static str, reduced: bool, window: &mut Window) -> f32 {
        let at = *self.opened.entry(key).or_insert_with(Instant::now);
        let k = if reduced { 1. } else { at.elapsed().as_secs_f32() / IN.as_secs_f32() };
        if k < 1. {
            window.request_animation_frame();
        }
        k.min(1.)
    }

    /// A modal drawn by a function: it fades in; it is gone at once when it closes.
    pub fn element(&mut self, key: &'static str, el: Option<AnyElement>, reduced: bool, window: &mut Window) -> Option<AnyElement> {
        let Some(el) = el else {
            self.opened.remove(key);
            return None;
        };
        let k = self.coming_in(key, reduced, window);
        Some(if k < 1. { layer(el, k).into_any_element() } else { el })
    }

    /// A modal that is a view: it fades in, and when it closes it is drawn a moment longer,
    /// fading out (and taking no clicks).
    pub fn view(&mut self, key: &'static str, v: Option<AnyView>, reduced: bool, window: &mut Window) -> Option<AnyElement> {
        if let Some(v) = v {
            self.closing.remove(key);
            self.views.insert(key, v.clone());
            let k = self.coming_in(key, reduced, window);
            return Some(if k < 1. { layer(v.into_any_element(), k).into_any_element() } else { v.into_any_element() });
        }
        self.opened.remove(key);
        if let Some(last) = self.views.remove(key)
            && !reduced
        {
            self.closing.insert(key, (last, Instant::now()));
        }
        let (last, at) = self.closing.get(key)?;
        let k = 1. - at.elapsed().as_secs_f32() / OUT.as_secs_f32();
        if k <= 0. {
            self.closing.remove(key);
            return None;
        }
        window.request_animation_frame();
        // A cover over it: what was clicked to close it cannot be clicked again.
        let cover = div().absolute().top_0().left_0().right_0().bottom_0().occlude();
        Some(layer(last.clone().into_any_element(), k).child(cover).into_any_element())
    }
}
