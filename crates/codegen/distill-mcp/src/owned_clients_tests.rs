use super::*;

#[test]
fn remove_if_same_leaves_a_successor_in_place() {
    let mut clients = OwnedClients::new();
    let first = Arc::new(McpClient::stub("srv"));
    let successor = Arc::new(McpClient::stub("srv"));
    clients.insert("srv".to_owned(), Arc::clone(&first));
    clients.insert("srv".to_owned(), Arc::clone(&successor));

    clients.remove_if_same("srv", &first);
    assert!(
        clients
            .get("srv")
            .is_some_and(|c| Arc::ptr_eq(c, &successor)),
        "a decision made against the old client must not evict the one installed after it"
    );
    clients.remove_if_same("srv", &successor);
    assert!(clients.get("srv").is_none());
}
