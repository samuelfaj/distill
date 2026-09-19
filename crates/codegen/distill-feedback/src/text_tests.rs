use super::*;

#[test]
fn derive_title_takes_the_first_non_blank_line_trimmed_to_eighty_chars() {
    let long_first_line = format!("{}\nsecond line", "é".repeat(100));
    let truncated = "é".repeat(MAX_TITLE_CHARS);
    let cases = [
        ("", FALLBACK_TITLE),
        (" \n\t\r\n", FALLBACK_TITLE),
        ("\n  \n  Leading blanks  \nsecond line", "Leading blanks"),
        ("  single line  ", "single line"),
        (long_first_line.as_str(), truncated.as_str()),
    ];

    for (text, expected) in cases {
        assert_eq!(derive_title(text), expected, "{text:?}");
    }
}

#[test]
fn post_text_joins_the_trimmed_title_and_details_with_a_blank_line() {
    assert_eq!(
        post_text(
            "  Wrong API used \n",
            "\n- The edit used the wrong API.\n\n"
        ),
        "Wrong API used\n\n- The edit used the wrong API."
    );
}
