use ic_base_types::{PrincipalId, PrincipalIdClass};
use ic_nns_test_utils::{
    common::NnsInitPayloadsBuilder,
    state_test_helpers::{
        registry_high_capacity_get_changes_since, setup_nns_canisters,
        state_machine_builder_for_nns_tests,
    },
};
use ic_registry_transport::pb::v1::HighCapacityRegistryGetChangesSinceResponse;
use std::str::FromStr;

#[test]
fn test_allow_opaque_caller() {
    // Step 1: Prepare the world.
    let state_machine = state_machine_builder_for_nns_tests().build();

    let nns_init_payloads = NnsInitPayloadsBuilder::new()
        .with_initial_invariant_compliant_mutations()
        .build();
    setup_nns_canisters(&state_machine, nns_init_payloads);

    // Step 2: Call the code under test.
    let sender = PrincipalId::from_str(
        // NNS root canister ID.
        "r7inp-6aaaa-aaaaa-aaabq-cai",
    )
    .unwrap();
    assert_eq!(sender.class(), Ok(PrincipalIdClass::Opaque));
    let response = registry_high_capacity_get_changes_since(&state_machine, sender, 0);

    // Step 3: Inspect results.
    let HighCapacityRegistryGetChangesSinceResponse {
        error,
        version,
        deltas,
    } = response;

    assert_eq!(error, None);
    // The important thing is that deltas is not empty. The exact number of
    // elements is not so important.
    assert_eq!(deltas.len(), 14);
    assert_eq!(version, 1);
}

#[test]
fn test_allow_self_authenticating_caller() {
    // Step 1: Prepare the world. (Same as the previous test.)
    let state_machine = state_machine_builder_for_nns_tests().build();

    let nns_init_payloads = NnsInitPayloadsBuilder::new()
        .with_initial_invariant_compliant_mutations()
        .build();
    setup_nns_canisters(&state_machine, nns_init_payloads);

    // Step 2: Call the code under test. Unlike the previous test, the sender is a
    // self-authenticatying principal, not an opaque principal.
    let sender =
        PrincipalId::from_str("ubktz-haghv-fqsdh-23fhi-3urex-bykoz-pvpfd-5rs6w-qpo3t-nf2dv-oae")
            .unwrap();
    assert_eq!(sender.class(), Ok(PrincipalIdClass::SelfAuthenticating));
    let response = registry_high_capacity_get_changes_since(&state_machine, sender, 0);

    // Step 3: Inspect results.
    let HighCapacityRegistryGetChangesSinceResponse {
        error,
        version,
        deltas,
    } = response;

    assert_eq!(error, None);
    // The important thing is that deltas is not empty. The exact number of
    // elements is not so important.
    assert_eq!(deltas.len(), 14);
    assert_eq!(version, 1);
}
