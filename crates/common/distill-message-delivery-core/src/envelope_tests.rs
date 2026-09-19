use super::*;

struct OpaqueGrant(u64);

#[test]
fn marker_constructors_set_principal_and_preserve_grant() {
    assert_eq!(
        DeliveryEnvelope::from_human(Operation::Queue, (), (), OpaqueGrant(11)).principal(),
        Principal::Human
    );
    let agent = DeliveryEnvelope::from_agent(Operation::Queue, (), (), OpaqueGrant(17));
    assert_eq!(agent.principal(), Principal::Agent);
    assert_eq!(agent.into_parts().3.0, 17);
}

#[test]
fn queue_operation_set_authorizes_queue_and_rejects_steer() {
    assert!(authorize_operation(OperationSet::QUEUE, Operation::Queue).is_ok());
    assert!(authorize_operation(OperationSet::QUEUE, Operation::Steer).is_err());
    assert!(authorize_operation(OperationSet::QUEUE_AND_STEER, Operation::Queue).is_ok());
    assert!(authorize_operation(OperationSet::QUEUE_AND_STEER, Operation::Steer).is_ok());
}

#[test]
fn queue_steer_and_interject_set_authorizes_three_and_rejects_interrupt() {
    let set = OperationSet::QUEUE_STEER_AND_INTERJECT;
    assert!(authorize_operation(set, Operation::Queue).is_ok());
    assert!(authorize_operation(set, Operation::Steer).is_ok());
    assert!(authorize_operation(set, Operation::Interject).is_ok());
    assert!(authorize_operation(set, Operation::InterruptAndSend).is_err());
    assert!(authorize_operation(OperationSet::QUEUE_AND_STEER, Operation::Interject).is_err());
}
