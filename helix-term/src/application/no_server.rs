//! Stands in for the session server where there is none: on Windows, and in integration tests,
//! whose test backend has no terminal to share.

use super::Application;

pub enum Server {}

pub enum ServerEvent {}

pub async fn next_event(_server: &mut Option<Server>) -> ServerEvent {
    std::future::pending().await
}

impl Application {
    pub(super) async fn handle_server_event(&mut self, event: ServerEvent) -> bool {
        match event {}
    }

    pub(super) fn update_clients(&mut self) -> bool {
        !self.editor.should_close()
    }

    pub(super) fn is_suspended(&self) -> bool {
        false
    }
}
