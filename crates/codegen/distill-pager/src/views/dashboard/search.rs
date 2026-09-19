use crate::views::dashboard::state::{DashboardState, Filter};

impl DashboardState {
    pub fn enter_search_mode(&mut self) {
        self.search_mode = true;
        self.set_list_focused(false);
        self.dispatch.set_text("");
        self.filter = Filter::None;
        self.error_toast = None;
        self.manual_scroll_active = false;
    }

    pub fn exit_search_mode(&mut self) {
        self.search_mode = false;
        self.dispatch.set_text("");
        self.filter = Filter::None;
        self.manual_scroll_active = false;
    }
}
