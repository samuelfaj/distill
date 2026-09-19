use crate::app::agent::{BgTaskState, BgTaskStatus, ScheduledTaskInfo};
use crate::app::roster::{RosterActivity, RosterEntry, RosterOrigin};

pub(crate) fn roster_entry(session_id: &str, activity: RosterActivity) -> RosterEntry {
    RosterEntry {
        session_id: session_id.to_owned(),
        title: Some("Remote work".to_owned()),
        cwd: "/tmp".to_owned(),
        is_worktree: false,
        session_kind: None,
        model_id: None,
        yolo: false,
        activity,
        last_turn_summary: None,
        resident: false,
        last_change_unix_ms: 0,
        origin: RosterOrigin::default(),
    }
}

pub(crate) fn running_bg_task(task_id: &str, is_monitor: bool) -> BgTaskState {
    BgTaskState {
        task_id: task_id.to_owned(),
        tool_call_id: String::new(),
        command: "sleep 99".to_owned(),
        description: None,
        cwd: String::new(),
        output_file: String::new(),
        status: BgTaskStatus::Running,
        start_time: std::time::SystemTime::now(),
        end_time: None,
        exit_code: None,
        signal: None,
        stdout: String::new(),
        stdout_line_count: 0,
        truncated: false,
        pending_kill: false,
        kill_requested_at: None,
        scrollback_entry_id: None,
        is_monitor,
        restored_from_replay: false,
    }
}

pub(crate) fn scheduled_loop(task_id: &str) -> ScheduledTaskInfo {
    ScheduledTaskInfo {
        task_id: task_id.to_owned(),
        prompt: "check things".to_owned(),
        human_schedule: "every 5m".to_owned(),
        created_at: std::time::Instant::now(),
        next_fire_at: None,
        tag: "loop".to_owned(),
        last_subagent_id: None,
    }
}
