"""
This module defines Bazel targets for the mainnet versions of the core NNS and SNS canisters.
"""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_file")

def mainnet_core_nns_canisters():
    """
    Provides Bazel targets for the **core** NNS canisters that are currently deployed to the mainnet.

    This includes: Lifeline, Root, Registry, Governance, ICP Ledger (Index, Archive), CMC, GTC, SNS-W.
    """

    http_file(
        name = "mainnet_nns_registry_canister",
        downloaded_file_path = "registry-canister.wasm.gz",
        sha256 = "641b6607bff6cd979fcf8af028831fc66d5f7f2bacc6ca498861cf7a8df4ec2c",
        url = "https://download.dfinity.systems/ic/87f48a595b6f666fbc7fe6ad4081aa98fd113d12/canisters/registry-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_nns_governance_canister",
        downloaded_file_path = "governance-canister.wasm.gz",
        sha256 = "6bd26499d2091c794ba814ad8d729fdff3c741b9dc0560760aecd7dd8db19d95",
        url = "https://download.dfinity.systems/ic/48e7e1f072c259810236488ff770c809f362fb63/canisters/governance-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_icp_ledger_canister",
        downloaded_file_path = "ledger-canister_notify-method.wasm.gz",
        sha256 = "cb16acc20a8948b68cc40b7d7328a53ec36c3cf713b3111921b49d009b98c5a5",
        url = "https://download.dfinity.systems/ic/c63b73baa88d65a45a60f4d7e0ec043f1fbf98be/canisters/ledger-canister_notify-method.wasm.gz",
    )

    http_file(
        name = "mainnet_icp_ledger-archive-node-canister",
        downloaded_file_path = "ledger-archive-node-canister.wasm.gz",
        sha256 = "569d498b638668733953a756f4a417533e1f513d1d41d55960abcd2f160d2aad",
        url = "https://download.dfinity.systems/ic/acd7e2928237a8e8bc1aa5e73efd47c8a8dfb556/canisters/ledger-archive-node-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_nns_root-canister",
        downloaded_file_path = "root-canister.wasm.gz",
        sha256 = "25af9548c5594dc95ba48e6140f4eaa0b59ef0c51ba939a73b1da160bed450ae",
        url = "https://download.dfinity.systems/ic/80261cb2fdf2e0b49c81040b97a7cac2dfec2251/canisters/root-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_nns_lifeline_canister",
        downloaded_file_path = "lifeline-canister.wasm.gz",
        sha256 = "6d3029dcd359f80cd64bb68bbc8bd95b810b08bd29b9ef1054118a5285f2abc5",
        url = "https://download.dfinity.systems/ic/87f48a595b6f666fbc7fe6ad4081aa98fd113d12/canisters/lifeline_canister.wasm.gz",
    )

    http_file(
        name = "mainnet_nns_genesis-token-canister",
        downloaded_file_path = "genesis-token-canister.wasm.gz",
        sha256 = "dd71862512af57e938e01810be016e17431912d9ca0ea3952bc04015eb02acc4",
        url = "https://download.dfinity.systems/ic/87f48a595b6f666fbc7fe6ad4081aa98fd113d12/canisters/genesis-token-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_nns_cycles-minting-canister",
        downloaded_file_path = "cycles-minting-canister.wasm.gz",
        sha256 = "fc1655302a81cd1b7148a6f66479b2830bd0d567d04780720e7bdd6c5b124fb5",
        url = "https://download.dfinity.systems/ic/e813c43a337a26c026c2f2ec19099bd2036e7280/canisters/cycles-minting-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_nns_sns-wasm-canister",
        downloaded_file_path = "sns-wasm-canister.wasm.gz",
        sha256 = "d31113c19ae571694a33b1689092b76d60c7e5722f27601291b67ef29b2ecee7",
        url = "https://download.dfinity.systems/ic/48e7e1f072c259810236488ff770c809f362fb63/canisters/sns-wasm-canister.wasm.gz",
    )

def mainnet_sns_canisters():
    """
    Provides Bazel targets for the latest SNS canisters published to the mainnet SNS-W.

    This includes: Root, SNS Governance, Swap, SNS Ledger (Index, Archive).
    """

    http_file(
        name = "mainnet_sns-root-canister",
        downloaded_file_path = "sns-root-canister.wasm.gz",
        sha256 = "8d479572f739d13ba05f1d98b834edb4bbd3f96abab90397a4701e6ed3142829",
        url = "https://download.dfinity.systems/ic/7a3db052ed4c3306272ed372039d9775f189c0bc/canisters/sns-root-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_sns-governance-canister",
        downloaded_file_path = "sns-governance-canister.wasm.gz",
        sha256 = "2bb65acf203f7816f30ca150dd72841f7971b6f94fd831cf418ada4a217b6a0d",
        url = "https://download.dfinity.systems/ic/7a3db052ed4c3306272ed372039d9775f189c0bc/canisters/sns-governance-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_sns-swap-canister",
        downloaded_file_path = "sns-swap-canister.wasm.gz",
        sha256 = "9531eafc54069e835636011e17a19a4709a3123c6a4d2f56c58a44c3cc31e1d6",
        url = "https://download.dfinity.systems/ic/48e7e1f072c259810236488ff770c809f362fb63/canisters/sns-swap-canister.wasm.gz",
    )

    http_file(
        name = "mainnet_ic-icrc1-ledger",
        downloaded_file_path = "ic-icrc1-ledger.wasm.gz",
        sha256 = "af8fc1469e553ac90f704521a97a1e3545c2b68049b4618a6549171b4ea4fba8",
        url = "https://download.dfinity.systems/ic/48e7e1f072c259810236488ff770c809f362fb63/canisters/ic-icrc1-ledger.wasm.gz",
    )

    http_file(
        name = "mainnet_ic-icrc1-archive",
        downloaded_file_path = "ic-icrc1-archive.wasm.gz",
        sha256 = "e691826056ac4ba1f95ebb9e24d06d6e787d4cb66fd36cb356358637d5041f49",
        url = "https://download.dfinity.systems/ic/48e7e1f072c259810236488ff770c809f362fb63/canisters/ic-icrc1-archive.wasm.gz",
    )

    http_file(
        name = "mainnet_ic-icrc1-index-ng",
        downloaded_file_path = "ic-icrc1-index-ng.wasm.gz",
        sha256 = "5ed3e211f820252ab11c063ca2f00948d13e7dd48d27cd6fcd508bec4a3b1f51",
        url = "https://download.dfinity.systems/ic/48e7e1f072c259810236488ff770c809f362fb63/canisters/ic-icrc1-index-ng.wasm.gz",
    )