//! Seccomp: child network filter (pre_exec) and process-wide namespace lockdown.

#[cfg(target_os = "linux")]
mod ns_lockdown {
    use libc::sock_filter;

    pub(super) const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    pub(super) const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    pub(super) const EPERM_VAL: u32 = 1;
    /// ENOSYS: libc treats clone3 as unavailable and falls back to legacy clone.
    pub(super) const ENOSYS_VAL: u32 = libc::ENOSYS as u32;
    #[cfg(target_arch = "x86_64")]
    pub(super) const X32_SYSCALL_BIT: u32 = 0x4000_0000;

    pub(super) const OFF_NR: u32 = 0;
    pub(super) const OFF_ARCH: u32 = 4;
    pub(super) const OFF_ARGS0_LO: u32 = 16; // LE low half of args[0]

    #[cfg(target_arch = "x86_64")]
    pub(super) const EXPECTED_ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    pub(super) const EXPECTED_ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub(super) const EXPECTED_ARCH: u32 = 0;

    pub(super) const CLONE_NAMESPACE_BITS: u32 = (libc::CLONE_NEWNS as u32)
        | (libc::CLONE_NEWCGROUP as u32)
        | (libc::CLONE_NEWUTS as u32)
        | (libc::CLONE_NEWIPC as u32)
        | (libc::CLONE_NEWUSER as u32)
        | (libc::CLONE_NEWPID as u32)
        | (libc::CLONE_NEWNET as u32)
        | (libc::CLONE_NEWTIME as u32);

    /// Linux `clone3` (arch-portable number; not always exported by libc).
    pub(super) const SYS_CLONE3: u32 = 435;

    pub(super) fn stmt(code: u32, k: u32) -> sock_filter {
        sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        }
    }

    pub(super) fn jump(code: u32, k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        }
    }

    /// Wrong-arch / x32 numbers must not skip the deny list.
    pub(super) fn push_arch_nr_gate(f: &mut Vec<sock_filter>) {
        use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};
        f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARCH));
        f.push(jump(BPF_JMP | BPF_JEQ | BPF_K, EXPECTED_ARCH, 1, 0));
        f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));
        f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR));
        #[cfg(target_arch = "x86_64")]
        {
            f.push(jump(
                BPF_JMP | libc::BPF_JSET | BPF_K,
                X32_SYSCALL_BIT,
                0,
                1,
            ));
            f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));
        }
    }

    fn mount_mutation_syscalls() -> [u32; 9] {
        use libc::{
            SYS_fsconfig, SYS_fsmount, SYS_fsopen, SYS_mount, SYS_mount_setattr, SYS_move_mount,
            SYS_pivot_root, SYS_umount2,
        };
        // open_tree is 428 in the architecture-generic Linux syscall table but is missing from some libc target headers
        const SYS_OPEN_TREE: u32 = 428;
        [
            SYS_mount as u32,
            SYS_umount2 as u32,
            SYS_pivot_root as u32,
            SYS_OPEN_TREE,
            SYS_move_mount as u32,
            SYS_fsopen as u32,
            SYS_fsconfig as u32,
            SYS_fsmount as u32,
            SYS_mount_setattr as u32,
        ]
    }

    #[cfg(test)]
    pub(super) fn mount_mutation_syscalls_for_test() -> [u32; 9] {
        mount_mutation_syscalls()
    }

    /// The mount API, `unshare`, `setns`, and legacy `clone(CLONE_NEW*)` return EPERM. `clone3` returns ENOSYS: its flags
    /// live in a pointed-to struct classic BPF cannot inspect. ENOSYS makes libc fall back to legacy clone for ordinary
    /// spawn, while direct malicious clone3 cannot create namespaces.
    pub fn build_namespace_lockdown_filter() -> Vec<sock_filter> {
        use libc::{
            BPF_ABS, BPF_JEQ, BPF_JMP, BPF_JSET, BPF_K, BPF_LD, BPF_RET, BPF_W, SYS_clone,
            SYS_setns, SYS_unshare,
        };

        let mount_mutations = mount_mutation_syscalls();
        let mut f = Vec::with_capacity(mount_mutations.len() * 2 + 22);
        push_arch_nr_gate(&mut f);
        for sys in mount_mutations
            .into_iter()
            .chain([SYS_unshare as u32, SYS_setns as u32])
        {
            f.push(jump(BPF_JMP | BPF_JEQ | BPF_K, sys, 0, 1));
            f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));
        }
        f.push(jump(BPF_JMP | BPF_JEQ | BPF_K, SYS_CLONE3, 0, 1));
        f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS_VAL));
        f.push(jump(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone as u32, 0, 3));
        f.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARGS0_LO));
        f.push(jump(BPF_JMP | BPF_JSET | BPF_K, CLONE_NAMESPACE_BITS, 0, 1));
        f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));
        f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
        f
    }

    #[cfg(test)]
    pub fn filter_jeq_immediates(filter: &[sock_filter]) -> Vec<u32> {
        use libc::{BPF_JEQ, BPF_JMP, BPF_K};
        let jeq = (BPF_JMP | BPF_JEQ | BPF_K) as u16;
        filter
            .iter()
            .filter(|i| i.code == jeq)
            .map(|i| i.k)
            .collect()
    }

    pub fn install(filter: &mut [sock_filter]) -> std::io::Result<()> {
        use libc::{
            PR_SET_NO_NEW_PRIVS, SECCOMP_FILTER_FLAG_TSYNC, SECCOMP_SET_MODE_FILTER, SYS_seccomp,
            prctl, sock_fprog,
        };

        let prog = sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_mut_ptr(),
        };

        // SAFETY: standard NO_NEW_PRIVS before seccomp.
        if unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: prog valid for the duration of the syscall.
        // rc: 0 ok; >0 TSYNC failing TID; -1 errno.
        let rc = unsafe {
            libc::syscall(
                SYS_seccomp,
                SECCOMP_SET_MODE_FILTER as libc::c_long,
                SECCOMP_FILTER_FLAG_TSYNC as libc::c_long,
                &prog as *const sock_fprog as *const libc::c_void,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        if rc > 0 {
            return Err(std::io::Error::other(format!(
                "seccomp TSYNC failed: thread {rc} could not install filter"
            )));
        }
        Err(std::io::Error::last_os_error())
    }
}

/// Connect/send equivalents, including `io_uring_*` / `sendmmsg` which never enter `SYS_connect` / `SYS_sendmsg`.
#[cfg(target_os = "linux")]
fn child_network_blocked_syscalls() -> [u32; 11] {
    use libc::{
        SYS_accept, SYS_accept4, SYS_bind, SYS_connect, SYS_io_uring_enter, SYS_io_uring_register,
        SYS_io_uring_setup, SYS_listen, SYS_sendmmsg, SYS_sendmsg, SYS_sendto,
    };
    [
        SYS_connect as u32,
        SYS_bind as u32,
        SYS_sendto as u32,
        SYS_sendmsg as u32,
        SYS_sendmmsg as u32,
        SYS_listen as u32,
        SYS_accept as u32,
        SYS_accept4 as u32,
        SYS_io_uring_setup as u32,
        SYS_io_uring_enter as u32,
        SYS_io_uring_register as u32,
    ]
}

#[cfg(target_os = "linux")]
fn build_child_network_filter() -> Vec<libc::sock_filter> {
    use libc::{BPF_JEQ, BPF_JMP, BPF_K, BPF_RET};
    use ns_lockdown::{EPERM_VAL, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO, jump, stmt};

    let blocked = child_network_blocked_syscalls();
    let mut f = Vec::with_capacity(blocked.len() + 10);
    ns_lockdown::push_arch_nr_gate(&mut f);
    let n = blocked.len();
    for (i, &sys) in blocked.iter().enumerate() {
        let remaining = n - i - 1;
        f.push(jump(BPF_JMP | BPF_JEQ | BPF_K, sys, remaining as u8 + 1, 0));
    }
    f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    f.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM_VAL));
    f
}

/// The child-network BPF program, built once in the parent process. `pre_exec` closures run between `fork` and `exec` in
/// a multi-threaded process. Another thread can hold the allocator lock at fork, so any heap allocation there can
/// deadlock the child. Install therefore only references this parent-built buffer.
#[cfg(target_os = "linux")]
pub fn prebuilt_child_network_filter() -> &'static [libc::sock_filter] {
    static FILTER: std::sync::OnceLock<Vec<libc::sock_filter>> = std::sync::OnceLock::new();
    FILTER.get_or_init(build_child_network_filter)
}

/// # Safety
/// After fork / before exec only. Async-signal-safe: two `prctl`s on the
/// parent-built program; never allocate, lock, log, format, or read env.
#[cfg(target_os = "linux")]
pub unsafe fn install_child_network_filter(filter: &[libc::sock_filter]) -> std::io::Result<()> {
    use libc::{PR_SET_NO_NEW_PRIVS, PR_SET_SECCOMP, SECCOMP_MODE_FILTER, prctl, sock_fprog};

    // PR_SET_SECCOMP copies the program into the kernel and never writes through this pointer; sock_fprog merely lacks a const field
    let prog = sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_ptr().cast_mut(),
    };
    if unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe {
        prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER as libc::c_ulong,
            &prog as *const _ as libc::c_ulong,
            0,
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Deny nested namespace creation (TSYNC). Legacy clone after clone3 ENOSYS.
/// # Safety
/// Process-wide; call after bwrap re-exec / at apply.
#[cfg(target_os = "linux")]
pub unsafe fn install_namespace_lockdown_filter() -> std::io::Result<()> {
    let mut filter = ns_lockdown::build_namespace_lockdown_filter();
    ns_lockdown::install(&mut filter)
}

/// They do not survive a daemon unlink/recreate, so the session-long guarantee is this per-spawn seccomp filter. Every
/// spawn of an approved user- or workspace-authored executable must call one of them instead of hand-rolling the
/// `pre_exec` install. No-op when the sandbox does not restrict child networking, and on non-Linux targets.
pub fn restrict_child_network(cmd: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    if crate::should_restrict_child_network() {
        let filter = prebuilt_child_network_filter();
        // SAFETY: the closure only runs prctl against the parent-built
        // program — async-signal-safe, no allocation or locking after fork.
        unsafe {
            cmd.pre_exec(move || install_child_network_filter(filter));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

/// Std twin of [`restrict_child_network`] with the same gate and parent-built filter.
pub fn restrict_child_network_std(cmd: &mut std::process::Command) {
    #[cfg(target_os = "linux")]
    if crate::should_restrict_child_network() {
        use std::os::unix::process::CommandExt;
        let filter = prebuilt_child_network_filter();
        // SAFETY: the closure only runs prctl against the parent-built
        // program — async-signal-safe, no allocation or locking after fork.
        unsafe {
            cmd.pre_exec(move || install_child_network_filter(filter));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

/// # Safety
/// Process-wide; call after bwrap re-exec / at apply.
#[cfg(not(target_os = "linux"))]
pub unsafe fn install_namespace_lockdown_filter() -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::ns_lockdown::*;
    use libc::{SYS_clone, SYS_setns, SYS_unshare, sock_filter};

    /// Minimal classic-BPF interpreter over synthetic seccomp_data fields.
    fn eval(filter: &[sock_filter], arch: u32, nr: u32, arg0_lo: u32) -> u32 {
        use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_JSET, BPF_K, BPF_LD, BPF_RET, BPF_W};
        let mut pc = 0usize;
        let mut a = 0u32;
        for _ in 0..filter.len().saturating_mul(2) {
            let Some(insn) = filter.get(pc) else { break };
            let op = insn.code as u32;
            if op == (BPF_LD | BPF_W | BPF_ABS) {
                a = match insn.k {
                    OFF_NR => nr,
                    OFF_ARCH => arch,
                    OFF_ARGS0_LO => arg0_lo,
                    _ => 0,
                };
                pc += 1;
            } else if op == (BPF_JMP | BPF_JEQ | BPF_K) {
                pc = if a == insn.k {
                    pc + 1 + insn.jt as usize
                } else {
                    pc + 1 + insn.jf as usize
                };
            } else if op == (BPF_JMP | BPF_JSET | BPF_K) {
                pc = if a & insn.k != 0 {
                    pc + 1 + insn.jt as usize
                } else {
                    pc + 1 + insn.jf as usize
                };
            } else if op == (BPF_RET | BPF_K) {
                return insn.k;
            } else {
                panic!("unsupported opcode {:#x} at {pc}", insn.code);
            }
            if pc >= filter.len() {
                panic!("pc out of range");
            }
        }
        panic!("filter did not RET");
    }

    fn is_allow(r: u32) -> bool {
        r == SECCOMP_RET_ALLOW
    }
    fn is_eperm(r: u32) -> bool {
        r == (SECCOMP_RET_ERRNO | EPERM_VAL)
    }
    fn is_enosys(r: u32) -> bool {
        r == (SECCOMP_RET_ERRNO | ENOSYS_VAL)
    }

    #[test]
    fn namespace_filter_targets_mount_and_namespace_mutations() {
        let f = build_namespace_lockdown_filter();
        let jeqs = filter_jeq_immediates(&f);
        for syscall in mount_mutation_syscalls_for_test() {
            assert!(jeqs.contains(&syscall), "mount syscall {syscall}: {jeqs:?}");
        }
        assert!(jeqs.contains(&(SYS_unshare as u32)), "{jeqs:?}");
        assert!(jeqs.contains(&(SYS_setns as u32)), "{jeqs:?}");
        assert!(jeqs.contains(&SYS_CLONE3), "{jeqs:?}");
        assert!(jeqs.contains(&(SYS_clone as u32)), "{jeqs:?}");
        assert!(jeqs.contains(&EXPECTED_ARCH), "{jeqs:?}");
    }

    #[test]
    fn bpf_eval_ordinary_clone_allowed_namespace_clone_denied() {
        let f = build_namespace_lockdown_filter();
        // Ordinary clone/fork flags (no NEW*)
        assert!(is_allow(eval(
            &f,
            EXPECTED_ARCH,
            SYS_clone as u32,
            0x11 /* SIGCHLD | CLONE_VM-ish low bits without NEW* */
        )));
        assert!(is_eperm(eval(
            &f,
            EXPECTED_ARCH,
            SYS_clone as u32,
            libc::CLONE_NEWUSER as u32
        )));
        assert!(is_eperm(eval(
            &f,
            EXPECTED_ARCH,
            SYS_clone as u32,
            libc::CLONE_NEWNS as u32
        )));
    }

    #[test]
    fn bpf_eval_mount_and_namespace_mutations_are_blocked() {
        let f = build_namespace_lockdown_filter();
        for syscall in mount_mutation_syscalls_for_test() {
            assert!(
                is_eperm(eval(&f, EXPECTED_ARCH, syscall, 0)),
                "mount syscall {syscall} must be EPERM"
            );
        }
        assert!(is_enosys(eval(&f, EXPECTED_ARCH, SYS_CLONE3, 0)));
        assert!(is_eperm(eval(&f, EXPECTED_ARCH, SYS_unshare as u32, 0)));
        assert!(is_eperm(eval(&f, EXPECTED_ARCH, SYS_setns as u32, 0)));
        assert!(is_allow(eval(&f, EXPECTED_ARCH, 0, 0)));
    }

    #[test]
    fn bpf_eval_wrong_arch_and_x32_denied() {
        let f = build_namespace_lockdown_filter();
        assert!(is_eperm(eval(&f, 0xdead_beef, SYS_clone as u32, 0)));
        #[cfg(target_arch = "x86_64")]
        {
            // x32: nr has high bit set
            assert!(is_eperm(eval(
                &f,
                EXPECTED_ARCH,
                (SYS_unshare as u32) | X32_SYSCALL_BIT,
                0
            )));
        }
    }

    #[test]
    fn namespace_bits_cover_user_ns_and_mount_ns() {
        assert_ne!(CLONE_NAMESPACE_BITS & (libc::CLONE_NEWUSER as u32), 0);
        assert_ne!(CLONE_NAMESPACE_BITS & (libc::CLONE_NEWNS as u32), 0);
        assert_ne!(CLONE_NAMESPACE_BITS & (libc::CLONE_NEWNET as u32), 0);
    }

    #[test]
    fn filter_ends_with_allow() {
        let f = build_namespace_lockdown_filter();
        assert_eq!(f.last().unwrap().k, SECCOMP_RET_ALLOW);
    }

    #[test]
    fn child_network_filter_blocks_connect_equivalents_allows_read_socket() {
        let f = super::build_child_network_filter();
        for sys in super::child_network_blocked_syscalls() {
            assert!(
                is_eperm(eval(&f, EXPECTED_ARCH, sys, 0)),
                "syscall {sys} must be EPERM"
            );
        }
        assert!(is_allow(eval(&f, EXPECTED_ARCH, libc::SYS_read as u32, 0)));
        assert!(is_allow(eval(
            &f,
            EXPECTED_ARCH,
            libc::SYS_socket as u32,
            0
        )));
    }

    #[test]
    fn child_network_filter_wrong_arch_and_x32_denied() {
        let f = super::build_child_network_filter();
        assert!(is_eperm(eval(&f, 0xdead_beef, libc::SYS_read as u32, 0)));
        #[cfg(target_arch = "x86_64")]
        {
            assert!(is_eperm(eval(
                &f,
                EXPECTED_ARCH,
                (libc::SYS_read as u32) | X32_SYSCALL_BIT,
                0
            )));
        }
    }
}
