// Modified for Distill by Samuel Fajreldines, 2026.
/// Group-kills `cmd` if the future is dropped before it completes, reaping git/jj grandchildren a leader-only `kill_on_drop` would miss.
/// `cmd` must already lead its own group/session (git/jj `setsid`); otherwise teardown degrades to `kill_on_drop`.
pub(crate) async fn output_killing_group_on_drop(
    cmd: std::process::Command,
) -> std::io::Result<std::process::Output> {
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);
    #[allow(clippy::disallowed_methods)] // enrolled into the kill group on the next line
    let child = cmd.spawn()?;
    let guard = GroupKillGuard::attach(&child);
    let output = child.wait_with_output().await?;
    guard.disarm();
    Ok(output)
}

#[must_use = "dropping the guard kills the process group; call `disarm` on success"]
struct GroupKillGuard(Option<std::sync::Arc<distill_tty_utils::ProcessGroup>>);

impl GroupKillGuard {
    fn attach(child: &tokio::process::Child) -> Self {
        let group = distill_tty_utils::ProcessGroup::new()
            .and_then(|mut group| group.attach(child).map(|()| group))
            .map(std::sync::Arc::new)
            .ok();
        if let Some(group) = &group {
            let _ = distill_tty_utils::global_process_scope().register(group);
        }
        Self(group)
    }

    fn disarm(mut self) {
        if let Some(group) = self.0.take() {
            let _ = group.preserve_descendants();
        }
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0.take() {
            let _ = group.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn captures_output_on_normal_completion() {
        let mut cmd = std::process::Command::new("printf");
        cmd.arg("hello");
        cmd.stdout(std::process::Stdio::piped());
        let output = output_killing_group_on_drop(cmd)
            .await
            .expect("spawn printf");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_future_group_kills_grandchildren() {
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let read_pid =
            || -> Option<i32> { std::fs::read_to_string(&pid_file).ok()?.trim().parse().ok() };

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pid_file.display()));
        distill_tty_utils::detach_std_command(&mut cmd); // setsid: child leads its own group

        {
            let fut = output_killing_group_on_drop(cmd);
            tokio::pin!(fut);
            tokio::select! {
                _ = &mut fut => panic!("gather completed before the grandchild started"),
                () = async { while read_pid().is_none() { tokio::time::sleep(Duration::from_millis(20)).await } } => {}
            }
        }

        let pid = read_pid().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("grandchild {pid} survived the dropped gather"));
    }
}
