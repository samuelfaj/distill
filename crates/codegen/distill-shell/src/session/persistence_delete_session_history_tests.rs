use super::{DeleteSessionError, SessionDeletion, classify_remote_delete};
use crate::remote::client::BackendError;

#[test]
fn remote_ok_reports_removed() {
    assert!(
        classify_remote_delete(Ok(())).unwrap(),
        "a 2xx delete must report that a remote copy was removed"
    );
}

#[test]
fn remote_404_is_treated_as_already_deleted() {
    let removed = classify_remote_delete(Err(BackendError::RequestFailed {
        status: 404,
        body: "not found".into(),
    }))
    .expect("a 404 means the remote copy is gone — deletion must stay idempotent");
    assert!(
        !removed,
        "a 404 must report that nothing was removed remotely"
    );
}

#[test]
fn remote_non_404_request_failure_aborts() {
    let res = classify_remote_delete(Err(BackendError::RequestFailed {
        status: 500,
        body: "boom".into(),
    }));
    assert!(matches!(res, Err(DeleteSessionError::Remote(_))));
}

#[test]
fn remote_auth_failure_aborts() {
    let res = classify_remote_delete(Err(BackendError::Auth("denied".into())));
    assert!(matches!(res, Err(DeleteSessionError::Remote(_))));
}

#[test]
fn any_removed_reflects_either_location() {
    assert!(!SessionDeletion::default().any_removed());
    assert!(
        SessionDeletion {
            local_removed: true,
            remote_removed: false,
        }
        .any_removed()
    );
    assert!(
        SessionDeletion {
            local_removed: false,
            remote_removed: true,
        }
        .any_removed(),
        "a remote-only delete must count as removed"
    );
}
