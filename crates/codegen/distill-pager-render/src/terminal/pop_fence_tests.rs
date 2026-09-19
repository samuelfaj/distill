use super::da1_reply_len;

/// kitty (trailing `;`, with and without the write-clipboard param), tmux, alacritty, xterm, VTE.
#[test]
fn accepts_real_replies() {
    let replies: &[&[u8]] = &[
        b"\x1b[?62;c",
        b"\x1b[?62;52;c",
        b"\x1b[?62;22c",
        b"\x1b[?1;2c",
        b"\x1b[?6c",
        b"\x1b[?64;1;2;6;9;15;16;17;18;21;22;28c",
        b"\x1b[?65;1;9c",
    ];
    for reply in replies {
        assert_eq!(
            Some(reply.len()),
            da1_reply_len(reply),
            "{:?}",
            String::from_utf8_lossy(reply)
        );
    }
}

#[test]
fn measures_only_the_reply_after_residue() {
    let tail = b"junk\x1b[100;5:3u\x1b[?62;c";
    assert_eq!(Some(b"\x1b[?62;c".len()), da1_reply_len(tail));
}

#[test]
fn rejects_non_replies() {
    let non_replies: &[&[u8]] = &[
        b"\x1b[100;5:3u",
        b"\x1b[>0;2500;1c",
        b"\x1b[?0u",
        b"\x1b[c",
        b"\x1b[?62;2",
        b"",
        b"\x1b[?6x",
    ];
    for bytes in non_replies {
        assert_eq!(
            None,
            da1_reply_len(bytes),
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}
