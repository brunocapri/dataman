//! Server-specific panels: the UI half of `diskman_core::server::ServerExtension`.
//!
//! A panel declares the extension it belongs to (`"lar"` for LAR). When a connection's
//! `ServerInfo` lists that extension, the panel's button lights up in the toolbar and the panel
//! opens in the right side panel (360 pt, 280–520, opaque `raise`). LAR's live queries plug in
//! here (`lar::register`).
//!
//! The toolbar shows the LAR buttons of [`LAR_BUTTONS`] on a LAR server (today only Live, ⌥1):
//! a button whose panel id is registered toggles that panel; one whose panel is not registered
//! is drawn disabled. Panels registered under other ids get a plain button after them.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{AnyView, App, Global, SharedString, Window};
use diskman_core::{Connection, ServerInfo};

pub trait ServerPanel: 'static {
    /// The `ServerExtension::id` this panel needs.
    fn extension(&self) -> &'static str;
    fn id(&self) -> &'static str;
    fn title(&self) -> SharedString;
    fn build(&self, conn: Arc<Connection>, window: &mut Window, cx: &mut App) -> AnyView;
    /// The side panel's width when this panel opens (`None`: keep the current one).
    fn width(&self) -> Option<f32> {
        None
    }
}

/// The toolbar's LAR group: (panel id, label, icon, shortcut, what it is).
pub const LAR_BUTTONS: [(&str, &str, crate::assets::Icon, &str, &str); 1] =
    [("live", "Live", crate::assets::Icon::Live, "⌥1", "Live queries: grids that update as data changes")];

#[derive(Default)]
pub struct PanelRegistry {
    panels: Vec<Rc<dyn ServerPanel>>,
}

impl Global for PanelRegistry {}

impl PanelRegistry {
    /// LAR's panels register here at startup (`lar::register`).
    pub fn register(&mut self, panel: impl ServerPanel) {
        self.panels.push(Rc::new(panel));
    }

    /// The panels that apply to a server.
    pub fn for_server(&self, info: &ServerInfo) -> Vec<Rc<dyn ServerPanel>> {
        self.panels.iter().filter(|p| info.has(p.extension())).cloned().collect()
    }

    /// The panel with this id, if it applies to the server.
    pub fn find(&self, info: &ServerInfo, id: &str) -> Option<Rc<dyn ServerPanel>> {
        self.panels.iter().find(|p| p.id() == id && info.has(p.extension())).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;

    impl ServerPanel for Dummy {
        fn extension(&self) -> &'static str {
            diskman_core::server::LAR
        }
        fn id(&self) -> &'static str {
            "dummy"
        }
        fn title(&self) -> SharedString {
            "Dummy".into()
        }
        fn build(&self, _: Arc<Connection>, _: &mut Window, _: &mut App) -> AnyView {
            unreachable!()
        }
    }

    #[test]
    fn panels_follow_detected_extensions() {
        let mut r = PanelRegistry::default();
        r.register(Dummy);
        let pg = ServerInfo { version: "PostgreSQL 18.6".into(), ..Default::default() };
        let lar = ServerInfo { version: "PostgreSQL 16.6 (LAR)".into(), is_lar: true, extensions: vec!["lar"], ..Default::default() };
        assert!(r.for_server(&pg).is_empty());
        assert_eq!(r.for_server(&lar).len(), 1);
        assert!(r.find(&lar, "dummy").is_some());
        assert!(r.find(&pg, "dummy").is_none());
        assert!(r.find(&lar, "live").is_none());
    }
}
