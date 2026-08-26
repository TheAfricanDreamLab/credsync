//! DoD 1: `proptest` encode/decode round-trip green for every wire type.
//!
//! A round-trip test alone is not sufficient — it still passes if encoder and decoder are broken
//! in matching ways. Golden fixtures (CS-5) pin the actual bytes and close that gap. What this
//! file proves is that no valid value is lost or altered by a trip through the codec.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_protocol::canonical;
use proptest::prelude::*;

/// Round-trips a value and asserts equality, then asserts the re-encoded bytes match.
macro_rules! roundtrip_test {
    ($name:ident, $strategy:expr) => {
        proptest! {
            #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]
            #[test]
            fn $name(value in $strategy) {
                let bytes = canonical::to_vec(&value).expect("encodes");
                let back = canonical::from_slice(&bytes).expect("decodes");
                prop_assert_eq!(&value, &back, "value changed across a round trip");

                // Encoding the decoded value must reproduce the same bytes. This is what makes
                // the round trip meaningful for checksums: not just "equal values" but
                // "identical bytes".
                let again = canonical::to_vec(&back).expect("re-encodes");
                prop_assert_eq!(bytes, again, "re-encoding produced different bytes");
            }
        }
    };
}

roundtrip_test!(scope_id_roundtrips, common::scope_id());
roundtrip_test!(entity_name_roundtrips, common::entity_name());
roundtrip_test!(entity_id_roundtrips, common::entity_id());
roundtrip_test!(command_name_roundtrips, common::command_name());
roundtrip_test!(reason_roundtrips, common::reason());
roundtrip_test!(hex_roundtrips, common::hex());
roundtrip_test!(command_id_roundtrips, common::command_id());
roundtrip_test!(seq_roundtrips, common::seq());
roundtrip_test!(row_version_roundtrips, common::row_version());
roundtrip_test!(cursor_roundtrips, common::cursor());
roundtrip_test!(protocol_version_roundtrips, common::protocol_version());
roundtrip_test!(schema_version_roundtrips, common::schema_version());
roundtrip_test!(limit_bytes_roundtrips, common::limit_bytes());
roundtrip_test!(op_roundtrips, common::op());
roundtrip_test!(status_roundtrips, common::status());
roundtrip_test!(conflict_class_roundtrips, common::conflict_class());
roundtrip_test!(snapshot_roundtrips, common::snapshot());
roundtrip_test!(payload_roundtrips, common::payload());
roundtrip_test!(change_roundtrips, common::change());
roundtrip_test!(batch_roundtrips, common::batch());
roundtrip_test!(command_roundtrips, common::command());
roundtrip_test!(command_result_roundtrips, common::command_result());
roundtrip_test!(pull_request_roundtrips, common::pull_request());
roundtrip_test!(pull_response_roundtrips, common::pull_response());
roundtrip_test!(push_request_roundtrips, common::push_request());
roundtrip_test!(push_response_roundtrips, common::push_response());
roundtrip_test!(bootstrap_row_roundtrips, common::bootstrap_row());
roundtrip_test!(bootstrap_response_roundtrips, common::bootstrap_response());
roundtrip_test!(
    entity_registration_roundtrips,
    common::entity_registration()
);
roundtrip_test!(forced_upgrade_roundtrips, common::forced_upgrade());

proptest! {
    /// The `op`/`snapshot` rule survives a round trip in both directions.
    #[test]
    fn change_op_snapshot_rule_holds(value in common::change()) {
        prop_assert!(value.validate().is_ok());
        let bytes = canonical::to_vec(&value).expect("encodes");
        let back: credsync_protocol::Change = canonical::from_slice(&bytes).expect("decodes");
        prop_assert!(back.validate().is_ok());
    }

    /// A rejection always carries its reason after a round trip.
    #[test]
    fn command_result_reason_rule_holds(value in common::command_result()) {
        prop_assert!(value.validate().is_ok());
        let bytes = canonical::to_vec(&value).expect("encodes");
        let back: credsync_protocol::CommandResult =
            canonical::from_slice(&bytes).expect("decodes");
        prop_assert!(back.validate().is_ok());
    }
}
