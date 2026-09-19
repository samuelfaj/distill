use std::sync::LazyLock;
use std::time::Instant;

pub mod stage {
    pub const CHILD_ENTER: &str = "child_enter";
    pub const SESSION_SPAWN: &str = "session_spawn";
    pub const SESSION_UP: &str = "session_up";
    pub const SB_BUILDER_DONE: &str = "sb_builder_done";
    pub const SB_AGENT_BUILT: &str = "sb_agent_built";
    pub const TURN_DONE: &str = "turn_done";
    pub const FLUSH_DONE: &str = "flush_done";
    pub const CHILD_DONE: &str = "child_done";
    pub const MOCK_REQ: &str = "mock_req";
}

pub const ENV: &str = "GROK_SUBAGENT_WATERFALL";
pub const LINE_PREFIX: &str = "WATERFALL";
pub const T0_LINE_PREFIX: &str = "WATERFALL-T0";

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn now_us() -> u128 {
    EPOCH.elapsed().as_micros()
}

pub fn mark(id: &str, stage: &str) {
    mark_with_clock(id, stage, now_us);
}

fn mark_with_clock(id: &str, stage: &str, clock: impl FnOnce() -> u128) {
    enum Sink {
        Off,
        Stderr,
        File(std::sync::Mutex<std::fs::File>),
    }
    static SINK: std::sync::OnceLock<Sink> = std::sync::OnceLock::new();
    let sink = SINK.get_or_init(|| match std::env::var(ENV) {
        Err(_) => Sink::Off,
        Ok(v) if v.starts_with('/') => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&v)
            .map(|f| Sink::File(std::sync::Mutex::new(f)))
            .unwrap_or(Sink::Stderr),
        Ok(_) => Sink::Stderr,
    });
    let file = match sink {
        Sink::Off => return,
        Sink::Stderr => None,
        Sink::File(f) => Some(f),
    };
    let t_us = clock();
    match file {
        None => eprintln!("{LINE_PREFIX} id={id} stage={stage} t_us={t_us}"),
        Some(f) => {
            use std::io::Write as _;
            if let Ok(mut f) = f.lock() {
                let _ = writeln!(f, "{LINE_PREFIX} id={id} stage={stage} t_us={t_us}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn disabled_sink_reads_no_clock() {
        let reads = Cell::new(0u32);
        mark_with_clock("swp-x", stage::SESSION_SPAWN, || {
            reads.set(reads.get() + 1);
            0
        });
        assert_eq!(reads.get(), 0, "disabled mark must not read the clock");
    }
}
