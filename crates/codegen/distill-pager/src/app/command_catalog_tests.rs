use super::*;

fn commands(names: &[&str]) -> Vec<acp::AvailableCommand> {
    names
        .iter()
        .map(|&name| acp::AvailableCommand::new(name, String::new()))
        .collect()
}

#[test]
fn command_name_diff_reports_added_and_removed_sorted() {
    let prev = commands(&["alpha", "beta"]);
    let next = commands(&["gamma", "alpha", "delta"]);
    assert_eq!(
        command_name_diff(&prev, &next),
        (
            vec!["delta".to_owned(), "gamma".to_owned()],
            vec!["beta".to_owned()]
        )
    );
}
