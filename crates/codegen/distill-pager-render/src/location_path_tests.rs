// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

#[test]
fn shorten_location_path_kerem_distill_home() {
    assert_eq!(
        shorten_location_path("~/.grok/worktrees/code-xai/dashboard-design").as_ref(),
        "~/.g/w/code-xai/dashboard-design"
    );
    assert_eq!(
        shorten_location_path("$GROK_HOME/worktrees/code-xai/dashboard-design").as_ref(),
        "$GROK_HOME/w/code-xai/dashboard-design"
    );
}

#[test]
fn shorten_location_path_figma_deep_home() {
    assert_eq!(
        shorten_location_path("~/D/A/B/folder/xai").as_ref(),
        "~/D/A/B/folder/xai"
    );
    assert_eq!(
        shorten_location_path("~/Documents/Archive/Backup/folder/xai").as_ref(),
        "~/D/A/B/folder/xai"
    );
}

#[test]
fn shorten_location_path_short_paths_leave_alone() {
    assert_eq!(shorten_location_path("").as_ref(), "");
    assert_eq!(shorten_location_path("~").as_ref(), "~");
    assert_eq!(shorten_location_path("~/src").as_ref(), "~/src");
    assert_eq!(shorten_location_path("~/src/repo").as_ref(), "~/src/repo");
    assert_eq!(shorten_location_path("~/.grok").as_ref(), "~/.grok");
    assert_eq!(
        shorten_location_path("~/.grok/worktrees").as_ref(),
        "~/.grok/worktrees"
    );
    assert_eq!(shorten_location_path("$GROK_HOME").as_ref(), "$GROK_HOME");
    assert_eq!(
        shorten_location_path("$GROK_HOME/worktrees").as_ref(),
        "$GROK_HOME/worktrees"
    );
    assert_eq!(shorten_location_path("/work/xai").as_ref(), "/work/xai");
    assert_eq!(shorten_location_path("relative").as_ref(), "relative");
    assert_eq!(shorten_location_path("a/b").as_ref(), "a/b");
}

#[test]
fn shorten_location_path_dotdirs() {
    assert_eq!(
        shorten_location_path("~/.config/nvim/lua").as_ref(),
        "~/.c/nvim/lua"
    );
    assert_eq!(
        shorten_location_path("/home/user/.local/share/app").as_ref(),
        "/h/u/.l/share/app"
    );
    assert_eq!(
        shorten_location_path("~/.g/w/keep/full").as_ref(),
        "~/.g/w/keep/full"
    );
    assert_eq!(
        shorten_location_path("/a/../keep/full").as_ref(),
        "/a/../keep/full"
    );
}

#[test]
fn shorten_location_path_multi_dot_dirs_are_not_traversal() {
    assert_eq!(
        shorten_location_path("/home/user/..cache/project/file").as_ref(),
        "/h/u/..c/project/file"
    );
    assert_eq!(
        shorten_location_path("/home/user/.hidden/project/file").as_ref(),
        "/h/u/.h/project/file"
    );
    assert_eq!(
        shorten_location_path("/home/user/...foo/project/file").as_ref(),
        "/h/u/...f/project/file"
    );
    assert_eq!(shorten_location_component("."), ".");
    assert_eq!(shorten_location_component(".."), "..");
    assert_eq!(shorten_location_component("..."), "...");
    assert_eq!(shorten_location_component(".grok"), ".g");
    assert_eq!(shorten_location_component("..cache"), "..c");
    assert_eq!(shorten_location_component("...foo"), "...f");
    assert_eq!(shorten_location_component("Documents"), "D");
}

#[test]
fn shorten_location_path_non_home() {
    assert_eq!(
        shorten_location_path("/work/xai/frontend/apps").as_ref(),
        "/w/x/frontend/apps"
    );
    assert_eq!(
        shorten_location_path("/deep/alpha/bravo/charlie/delta").as_ref(),
        "/d/a/b/charlie/delta"
    );
    assert_eq!(
        shorten_location_path("foo/bar/baz/qux").as_ref(),
        "f/b/baz/qux"
    );
    assert_eq!(
        shorten_location_path("C:\\Users\\alice\\folder\\xai").as_ref(),
        "C:\\U\\a\\folder\\xai"
    );
}

#[test]
fn shorten_location_path_windows_drive_relative_vs_rooted() {
    assert_eq!(
        shorten_location_path(r"C:foo\bar\baz").as_ref(),
        r"C:f\bar\baz"
    );
    assert_eq!(
        shorten_location_path("C:foo/bar/baz").as_ref(),
        "C:f/bar/baz"
    );
    assert_eq!(shorten_location_path(r"C:foo\bar").as_ref(), r"C:foo\bar");
    assert_eq!(
        shorten_location_path(r"\foo\bar\baz").as_ref(),
        r"\f\bar\baz"
    );
    assert_eq!(shorten_location_path("/foo/bar/baz").as_ref(), "/f/bar/baz");
    assert_eq!(shorten_location_path(r"\foo\bar").as_ref(), r"\foo\bar");
    assert_eq!(
        shorten_location_path(r"C:\foo\bar\baz").as_ref(),
        r"C:\f\bar\baz"
    );
    assert_eq!(
        shorten_location_path(r"C:\Users\alice\folder\xai").as_ref(),
        r"C:\U\a\folder\xai"
    );
}

/// `/work/team\notes/repo` is three Unix components; last-two keeps `team\notes`.
#[cfg(not(windows))]
#[test]
fn shorten_location_path_unix_backslash_stays_in_component() {
    assert_eq!(
        shorten_location_path("/work/team\\notes/repo").as_ref(),
        "/w/team\\notes/repo"
    );
    assert_eq!(
        shorten_location_path("/work/xai/team\\notes/repo").as_ref(),
        "/w/x/team\\notes/repo"
    );
}

#[test]
fn shorten_location_path_does_not_eat_distill_home_lookalike() {
    assert_eq!(
        shorten_location_path("$GROK_HOME_BACKUP/a/b/c").as_ref(),
        "$/a/b/c"
    );
}

#[test]
fn shorten_location_path_keeps_unc_share_root() {
    assert_eq!(
        shorten_location_path(r"\\fileserver\projects\archive\backup\folder\xai").as_ref(),
        r"\\fileserver\projects\a\b\folder\xai"
    );
    assert_eq!(
        shorten_location_path(r"\\server\share\folder\xai").as_ref(),
        r"\\server\share\folder\xai"
    );
    assert_eq!(
        shorten_location_path(r"\\server\share").as_ref(),
        r"\\server\share"
    );
    assert_eq!(
        shorten_location_path("//host/share/archive/backup/folder/xai").as_ref(),
        "//host/share/a/b/folder/xai"
    );
    assert_eq!(
        shorten_location_path("//host/share/folder/xai").as_ref(),
        "//host/share/folder/xai"
    );
    assert_eq!(
        shorten_location_path("///foo/bar/baz/qux").as_ref(),
        "/f/b/baz/qux"
    );
    // Extra separators are dropped by the one split, not a second remainder scan.
    assert_eq!(
        shorten_location_path(r"\\fileserver\\projects\archive\\backup\folder\xai").as_ref(),
        r"\\fileserver\projects\a\b\folder\xai"
    );
    assert_eq!(
        shorten_location_path(r"\\?\UNC\fileserver\projects\archive\folder\xai").as_ref(),
        r"\\?\UNC\fileserver\projects\a\folder\xai"
    );
    assert_eq!(
        shorten_location_path(r"\\?\UNC\fileserver\projects\archive\backup\folder\xai").as_ref(),
        r"\\?\UNC\fileserver\projects\a\b\folder\xai"
    );
    assert_eq!(
        shorten_location_path(r"\\?\UNC\fileserver\projects\folder\xai").as_ref(),
        r"\\?\UNC\fileserver\projects\folder\xai"
    );
    assert_eq!(
        shorten_location_path(r"\\?\UNC\fileserver\projects").as_ref(),
        r"\\?\UNC\fileserver\projects"
    );
    assert_eq!(
        shorten_location_path(r"\\?\unc\fileserver\projects\archive\folder\xai").as_ref(),
        r"\\?\unc\fileserver\projects\a\folder\xai"
    );
}

/// Host-only UNC (`\\server` / `//host`) has one component after the prefix, so
/// `parts.len() <= KEEP_FULL` returns the input unchanged. There is no separate
/// share-missing fallback after the one-pass parse.
#[test]
fn shorten_location_path_unc_host_without_share_unchanged() {
    assert_eq!(
        shorten_location_path(r"\\fileserver").as_ref(),
        r"\\fileserver"
    );
    assert_eq!(shorten_location_path("//host").as_ref(), "//host");
    assert_eq!(
        shorten_location_path(r"\\fileserver\").as_ref(),
        r"\\fileserver\"
    );
    assert_eq!(shorten_location_path("//host/").as_ref(), "//host/");
}
