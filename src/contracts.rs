//! Contract identification and labels, mirroring `app/contracts.py`.

use alloy_primitives::Address;
use serde::Serialize;
use tempo_primitives::TempoAddressExt as _;

use crate::decoder::checksum_address;

/// An address known by name, and the built-in ABIs that describe it. The fee
/// manager has two: its own interface says nothing about the AMM it runs.
struct Known {
    address: &'static str,
    label: &'static str,
    /// `precompile`, or `contract` for an ordinary deployment at a fixed address.
    kind: &'static str,
    abis: &'static [&'static str],
}

const fn precompile(
    address: &'static str,
    label: &'static str,
    abis: &'static [&'static str],
) -> Known {
    Known {
        address,
        label,
        kind: "precompile",
        abis,
    }
}

const fn contract(
    address: &'static str,
    label: &'static str,
    abis: &'static [&'static str],
) -> Known {
    Known {
        address,
        label,
        kind: "contract",
        abis,
    }
}

/// Tempo's precompiles, then ordinary contracts at fixed addresses. Matched
/// ignoring case.
const KNOWN: &[Known] = &[
    precompile(
        "0xfeEC000000000000000000000000000000000000",
        "Fee Manager",
        &["fee_manager", "fee_amm"],
    ),
    precompile(
        "0x403c000000000000000000000000000000000000",
        "TIP-403 Registry",
        &["tip403_registry"],
    ),
    precompile(
        "0x20Fc000000000000000000000000000000000000",
        "TIP-20 Factory",
        &["tip20_factory"],
    ),
    precompile(
        "0x4D50500000000000000000000000000000000000",
        "TIP-20 Channel Reserve",
        &["tip20_channel_reserve"],
    ),
    precompile(
        "0xDEc0000000000000000000000000000000000000",
        "Stablecoin DEX",
        &["stablecoin_dex"],
    ),
    precompile(
        "0x4e4F4E4345000000000000000000000000000000",
        "Nonce Manager",
        &["nonce"],
    ),
    precompile(
        "0xCccCcCCC00000000000000000000000000000000",
        "Validator Config V1 (legacy)",
        &["validator_config"],
    ),
    precompile(
        "0xCccCcCCC00000000000000000000000000000001",
        "Validator Config",
        &["validator_config_v2"],
    ),
    precompile(
        "0xaAAAaaAA00000000000000000000000000000000",
        "Account Keychain",
        &["account_keychain"],
    ),
    precompile(
        "0xFDC0000000000000000000000000000000000000",
        "Address Registry",
        &["address_registry"],
    ),
    precompile(
        "0x5165300000000000000000000000000000000000",
        "Signature Verifier",
        &["signature_verifier"],
    ),
    precompile(
        "0xB10C000000000000000000000000000000000000",
        "Receive Policy Guard",
        &["receive_policy_guard"],
    ),
    precompile(
        "0x1060000000000000000000000000000000000000",
        "Storage Credits",
        &["storage_credits"],
    ),
    precompile(
        "0xC077e00000000000000000000000000000000000",
        "Current Committee",
        &["current_committee"],
    ),
    precompile(
        "0x5AF2000000000000000000000000000000000000",
        "Zone Factory",
        &["zone_factory"],
    ),
    contract(
        "0xcA11bde05977b3631167028862bE2a173976CA11",
        "Multicall3",
        &["multicall3"],
    ),
    contract(
        "0x000000000022D473030F116dDEE9F6B43aC78BA3",
        "Permit2",
        &["permit2"],
    ),
    contract(
        "0xba5Ed099633D3B313e4D5F7bdc1305d3c28ba5Ed",
        "CreateX",
        &["createx"],
    ),
    contract(
        "0x0000000000000000000000000000000000000a00",
        "Anchoring",
        &["anchoring"],
    ),
    contract(
        "0x5AD1000000000000000000000000000000000000",
        "Zone Portal Implementation",
        &["zone_portal"],
    ),
];

fn known(addr: &str) -> Option<&'static Known> {
    let addr = addr.trim();
    KNOWN.iter().find(|k| k.address.eq_ignore_ascii_case(addr))
}

/// The zone a portal address numbers, if it is one. The prefix alone, zone id
/// 0, is no portal: ids start at 1.
fn zone_portal_id(addr: &str) -> Option<u64> {
    addr.trim().parse::<Address>().ok()?.zone_portal_id()
}

/// Tokens genesis puts at reserved addresses, spelled as metadata rows store
/// them. Listed only so rows an older build named from a table can be refreshed.
pub const RESERVED_TOKENS: [&str; 4] = [
    "0x20C0000000000000000000000000000000000000",
    "0x20C0000000000000000000000000000000000001",
    "0x20C0000000000000000000000000000000000002",
    "0x20C0000000000000000000000000000000000003",
];

/// TIP-20 native token prefix: all StdTokens start with 0x20C000...
const TIP20_TOKEN_PREFIX: &str = "0x20c0000000000000000";

pub fn is_tip20_token(addr: &str) -> bool {
    addr.trim().to_lowercase().starts_with(TIP20_TOKEN_PREFIX)
}

/// The name of a contract at a fixed address, or of the zone portal an address numbers.
pub fn get_contract_name(addr: &str) -> Option<String> {
    if let Some(k) = known(addr) {
        return Some(k.label.to_string());
    }
    zone_portal_id(addr).map(|id| format!("Zone Portal #{id}"))
}

#[derive(Debug, Serialize)]
pub struct AddressInfo {
    #[serde(rename = "type")]
    pub kind: String,
    pub label: Option<String>,
}

pub fn identify_address(addr: &str) -> AddressInfo {
    let label = get_contract_name(addr);
    let kind = match known(addr) {
        Some(k) => k.kind,
        None if label.is_some() => "contract", // a zone portal
        None if is_tip20_token(addr) => "token",
        None => "eoa",
    };
    AddressInfo {
        kind: kind.into(),
        label,
    }
}

/// Which built-in ABIs describe the contract at `addr`, by the names the
/// decoder's registry loads them under.
pub fn abis_for_address(addr: &str) -> &'static [&'static str] {
    if let Some(k) = known(addr) {
        return k.abis;
    }
    // Every TIP-20 is the same interface at a different address.
    if is_tip20_token(addr) {
        return &["tip20", "tip20_roles_auth"];
    }
    if zone_portal_id(addr).is_some() {
        return &["zone_portal"];
    }
    &[]
}

/// Named contracts whose name contains `query`, as `(address, name)` pairs.
/// Sorted by match quality then alphabetically, so the same query always
/// produces the same list — a hash map's order is not one.
pub fn search_named(query: &str, limit: usize) -> Vec<(String, String)> {
    let query = query.trim().to_lowercase();
    if query.len() < 2 {
        return Vec::new();
    }
    let mut matches: Vec<(u8, &str, &str)> = KNOWN
        .iter()
        .filter_map(|k| {
            let lowered = k.label.to_lowercase();
            let rank = if lowered == query {
                0
            } else if lowered.starts_with(&query) {
                1
            } else if lowered.contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, k.label, k.address))
        })
        .collect();
    matches.sort();
    matches.truncate(limit);
    matches
        .into_iter()
        .map(|(_, name, address)| (checksum_address(address), name.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every ABI the table names must be one the registry actually loads.
    #[test]
    fn every_named_abi_is_registered() {
        for k in KNOWN {
            assert!(!k.abis.is_empty(), "{} has no ABI to show", k.label);
            for name in k.abis {
                assert!(
                    crate::decoder::REGISTRY.contract(name).is_some(),
                    "{} names `{name}`, which the registry does not load",
                    k.label
                );
            }
        }
    }

    /// The lookup must not care how an address is spelled.
    #[test]
    fn abis_are_found_however_the_address_is_spelled() {
        let fee_manager = "0xfeEC000000000000000000000000000000000000";
        assert_eq!(abis_for_address(fee_manager), ["fee_manager", "fee_amm"]);
        assert_eq!(
            abis_for_address(&fee_manager.to_lowercase()),
            ["fee_manager", "fee_amm"]
        );
        assert_eq!(
            abis_for_address(&fee_manager.to_uppercase().replace("0X", "0x")),
            ["fee_manager", "fee_amm"]
        );
    }

    /// A TIP-20 is recognised by its prefix, not by an entry per token.
    #[test]
    fn every_tip20_gets_the_token_interface() {
        assert_eq!(
            abis_for_address("0x20c0000000000000000000000000000000000042"),
            ["tip20", "tip20_roles_auth"]
        );
        assert!(abis_for_address("0x1111111111111111111111111111111111111111").is_empty());
    }

    /// The repair matches these against stored addresses without normalizing.
    #[test]
    fn reserved_tokens_are_spelled_as_they_are_stored() {
        for address in RESERVED_TOKENS {
            assert_eq!(checksum_address(address), address);
        }
    }
}
