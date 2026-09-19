use std::sync::Arc;

use super::*;

fn hit(id: &str) -> SearchSessionHit {
    SearchSessionHit {
        session_id: id.to_string(),
        cwd: "/ws".to_string(),
        summary: format!("title {id}"),
        updated_at: "2026-03-01T00:00:00Z".to_string(),
        score: 1.0,
        matched_fields: vec!["content".to_string()],
        snippet: None,
    }
}

fn fetch_from(
    rows: Arc<Vec<(SearchSessionHit, ClassifiedSessionKind)>>,
    page_size: usize,
) -> impl FnMut(usize, usize) -> std::future::Ready<io::Result<ClassifiedPage>> {
    move |offset, _batch| {
        let start = offset.min(rows.len());
        let end = (start + page_size).min(rows.len());
        std::future::ready(Ok(ClassifiedPage {
            hits: rows.get(start..end).unwrap_or(&[]).to_vec(),
            has_more: end < rows.len(),
            bootstrapping: false,
        }))
    }
}

fn fixture() -> Arc<Vec<(SearchSessionHit, ClassifiedSessionKind)>> {
    Arc::new(vec![
        (hit("h1"), ClassifiedSessionKind::Headless),
        (hit("i1"), ClassifiedSessionKind::Interactive),
        (hit("h2"), ClassifiedSessionKind::Headless),
        (hit("i2"), ClassifiedSessionKind::Interactive),
        (hit("i3"), ClassifiedSessionKind::Interactive),
    ])
}

fn ids(resp: &SearchSessionsResponse) -> Vec<&str> {
    resp.results.iter().map(|h| h.session_id.as_str()).collect()
}

fn fetch_with_bootstrap_states(
    rows: Arc<Vec<(SearchSessionHit, ClassifiedSessionKind)>>,
) -> impl FnMut(usize, usize) -> std::future::Ready<io::Result<ClassifiedPage>> {
    move |offset, _batch| {
        let end = (offset + 1).min(rows.len());
        std::future::ready(Ok(ClassifiedPage {
            hits: rows
                .get(offset.min(rows.len())..end)
                .unwrap_or(&[])
                .to_vec(),
            has_more: end < rows.len(),
            bootstrapping: offset > 0,
        }))
    }
}

#[tokio::test]
async fn unknown_hits_follow_policy_semantics() {
    let rows = Arc::new(vec![(hit("unknown"), ClassifiedSessionKind::Unknown)]);
    for policy in [HeadlessPolicy::Exclude, HeadlessPolicy::Only] {
        let resp = walk_admitted_window(fetch_from(rows.clone(), 1), 0, 1, policy)
            .await
            .unwrap();
        assert!(resp.results.is_empty());
        assert_eq!(resp.total_estimate, Some(0));
    }
    let included = walk_admitted_window(fetch_from(rows, 1), 0, 1, HeadlessPolicy::Include)
        .await
        .unwrap();
    assert_eq!(ids(&included), ["unknown"]);
}

#[tokio::test]
async fn later_page_bootstrapping_is_preserved() {
    let resp = walk_admitted_window(
        fetch_with_bootstrap_states(fixture()),
        0,
        2,
        HeadlessPolicy::Only,
    )
    .await
    .unwrap();
    assert!(resp.bootstrapping);
}

#[tokio::test]
async fn walk_stops_at_the_authoritative_lookup_cap() {
    let rows = Arc::new(
        (0..MAX_CLASSIFIED_HITS + 25)
            .map(|index| (hit(&format!("u{index}")), ClassifiedSessionKind::Unknown))
            .collect(),
    );
    let resp = walk_admitted_window(
        fetch_from(rows, WALK_BATCH),
        0,
        MAX_SEARCH_RESULTS,
        HeadlessPolicy::Only,
    )
    .await
    .unwrap();
    assert!(resp.results.is_empty());
    assert_eq!(resp.next_offset, None);
    assert_eq!(resp.total_estimate, None);
}

#[tokio::test]
async fn cap_truncated_last_page_is_not_exact_total() {
    let mut rows: Vec<_> = (0..1000)
        .map(|index| (hit(&format!("u{index}")), ClassifiedSessionKind::Unknown))
        .collect();
    rows.extend(
        (0..200).map(|index| (hit(&format!("mid{index}")), ClassifiedSessionKind::Unknown)),
    );
    rows.extend((0..50).map(|index| (hit(&format!("h{index}")), ClassifiedSessionKind::Headless)));
    let resp = walk_admitted_window(
        fetch_from(Arc::new(rows), 250),
        0,
        MAX_SEARCH_RESULTS,
        HeadlessPolicy::Only,
    )
    .await
    .unwrap();
    assert!(resp.results.is_empty(), "headless hits sit past the cap");
    assert_eq!(resp.next_offset, None);
    assert_eq!(
        resp.total_estimate, None,
        "truncated tail must not look like exhaustion"
    );
}

#[test]
fn zero_limit_is_rejected_before_pagination() {
    assert!(validate_search_window(0, 0).is_err());
}

#[tokio::test]
async fn only_walks_past_interactive_until_window_filled() {
    let rows = fixture();

    let resp = walk_admitted_window(fetch_from(rows.clone(), 2), 0, 1, HeadlessPolicy::Only)
        .await
        .unwrap();
    assert_eq!(ids(&resp), ["h1"]);
    assert_eq!(resp.next_offset, Some(1), "h2 exists past the window");
    assert_eq!(resp.total_estimate, None);

    let resp = walk_admitted_window(fetch_from(rows, 2), 1, 1, HeadlessPolicy::Only)
        .await
        .unwrap();
    assert_eq!(ids(&resp), ["h2"]);
    assert_eq!(resp.next_offset, None);
    assert_eq!(resp.total_estimate, Some(2), "exact filtered total");
}
