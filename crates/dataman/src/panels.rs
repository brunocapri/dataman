use std::rc::Rc;
use std::sync::Arc;

use gpui::{AnyView, App, Global, SharedString, Window};
use dataman_core::{Connection, ServerInfo};

pub trait ServerPanel: 'static {
    fn extension(&self) -> &'static str;
    fn id(&self) -> &'static str;
    fn title(&self) -> SharedString;
    fn build(&self, conn: Arc<Connection>, window: &mut Window, cx: &mut App) -> AnyView;
    fn width(&self) -> Option<f32> {
        None
    }
}

pub const LAR_BUTTONS: [(&str, &str, crate::assets::Icon, &str, &str); 1] =
    [("live", "Live", crate::assets::Icon::Live, "⌥1", "Live queries: grids that update as data changes")];

#[derive(Default)]
pub struct PanelRegistry {
    panels: Vec<Rc<dyn ServerPanel>>,
}

impl Global for PanelRegistry {}

impl PanelRegistry {
    pub fn register(&mut self, panel: impl ServerPanel) {
        self.panels.push(Rc::new(panel));
    }

    pub fn for_server(&self, info: &ServerInfo) -> Vec<Rc<dyn ServerPanel>> {
        self.panels.iter().filter(|p| info.has(p.extension())).cloned().collect()
    }

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
            dataman_core::server::LAR
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
