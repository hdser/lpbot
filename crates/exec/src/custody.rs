//! Checks on the custody itself, made before the executor sends anything
//! and again at every start (`docs/DESIGN.md`, "Smart contracts", the
//! lessons of 2026).
//!
//! Two incidents shape these checks. In June 2026 a bug in Zodiac's shared
//! `SignatureChecker` (an EIP-1271 call whose failure was not checked) let
//! attackers act through Delay v1.1.0 and Roles v2.1.0 modules; Gnosis Pay
//! lost about $1.5M. The audit of the fix names the patched mastercopies:
//! Roles v2.1.1 and Delay v1.1.1. In November 2026 about $6.45M left a Safe
//! through its Roles module with no owner signing: a role member's key was
//! most likely compromised and the policy allowed a transfer its owners had
//! not meant to allow. So, before any transaction:
//!
//! - the roles modifier is a proxy of the patched Roles v2.1.1 mastercopy,
//!   and nothing else;
//! - the Safe has exactly one module enabled, that modifier (no Delay, no
//!   module installed elsewhere);
//! - every address that may act through the modifier is a plain account
//!   with no code, so no contract signature (the June bug's path) can ever
//!   speak for it.
//!
//! What the policy allows is checked by its fork test (`docs/PLAN.md`, M4),
//! not here.

use alloy_primitives::{address, Address};

/// Zodiac Roles v2.1.1, patched for the June 2026 signature bug: the only
/// mastercopy the roles modifier may point at. From Gnosis Guild's registry
/// (`zodiac-deployments`, `mastercopies/roles/2.1.1`) and the audit of the
/// patch (12 June 2026).
pub const ROLES_V2_1_1: Address = address!("F2964CE6161ce0e75964Fe7927cE114cb0B283D5");
/// Zodiac Roles v2.1.0: vulnerable to the June 2026 signature bug.
pub const ROLES_V2_1_0: Address = address!("9646fDAD06d3e24444381f44362a3B0eB343D337");
/// Zodiac Delay v1.1.0: vulnerable; the bot never uses a Delay module.
pub const DELAY_V1_1_0: Address = address!("01F8cabB808D7dE0dF4202D4B60C8310d2f1339b");
/// Zodiac Delay v1.1.1, patched; still not one the bot uses.
pub const DELAY_V1_1_1: Address = address!("824175b945838d127C1ca83CbcE11D8e44F6dF01");

/// The runtime code of an EIP-1167 minimal proxy, as Zodiac's module proxy
/// factory deploys: these bytes, the target's 20, then the tail.
const PROXY_HEAD: [u8; 10] = [0x36, 0x3d, 0x3d, 0x37, 0x3d, 0x3d, 0x3d, 0x36, 0x3d, 0x73];
const PROXY_TAIL: [u8; 15] = [
    0x5a, 0xf4, 0x3d, 0x82, 0x80, 0x3e, 0x90, 0x3d, 0x91, 0x60, 0x2b, 0x57, 0xfd, 0x5b, 0xf3,
];

/// The contract a minimal proxy forwards to, if `code` is one.
pub fn proxy_target(code: &[u8]) -> Option<Address> {
    (code.len() == PROXY_HEAD.len() + 20 + PROXY_TAIL.len()
        && code.starts_with(&PROXY_HEAD)
        && code.ends_with(&PROXY_TAIL))
    .then(|| Address::from_slice(&code[PROXY_HEAD.len()..PROXY_HEAD.len() + 20]))
}

/// Why the custody is not fit to send through.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CustodyError {
    #[error(
        "the roles modifier {0} is not a minimal proxy; it must be a proxy of Roles v2.1.1 \
         ({ROLES_V2_1_1}) deployed through Zodiac's module factory"
    )]
    NotAProxy(Address),
    #[error(
        "the roles modifier {modifier} runs Roles v2.1.0 ({ROLES_V2_1_0}), which the June 2026 \
         signature bug affects: deploy a new modifier on v2.1.1 ({ROLES_V2_1_1}), apply the \
         policy to it, enable it and disable the old one, before anything is sent"
    )]
    Vulnerable { modifier: Address },
    #[error(
        "the roles modifier {modifier} runs {target}, which is not Roles v2.1.1 \
         ({ROLES_V2_1_1}); only that mastercopy is accepted"
    )]
    Unknown { modifier: Address, target: Address },
    #[error(
        "the Safe has {0} module(s) enabled; it must have exactly one, the roles modifier. \
         Disable every other module from the Safe, as an owner, before anything is sent"
    )]
    Modules(usize),
    #[error(
        "the Safe's module is {found}, not the roles modifier {expected}: disable it and enable \
         the modifier"
    )]
    WrongModule { found: Address, expected: Address },
    #[error(
        "{0} may act through the roles modifier and has code: only plain accounts may (a \
         contract, or an account delegated by EIP-7702, can be made to sign for itself, which \
         is how the June 2026 bug was used). Remove it from the modifier"
    )]
    ContractMember(Address),
}

/// The roles modifier must be a proxy of the patched mastercopy.
pub fn check_roles_modifier(modifier: Address, code: &[u8]) -> Result<(), CustodyError> {
    match proxy_target(code) {
        None => Err(CustodyError::NotAProxy(modifier)),
        Some(target) if target == ROLES_V2_1_1 => Ok(()),
        Some(target) if target == ROLES_V2_1_0 => Err(CustodyError::Vulnerable { modifier }),
        Some(target) => Err(CustodyError::Unknown { modifier, target }),
    }
}

/// The Safe's enabled modules must be exactly the roles modifier.
pub fn check_safe_modules(enabled: &[Address], modifier: Address) -> Result<(), CustodyError> {
    match enabled {
        [only] if *only == modifier => Ok(()),
        [only] => Err(CustodyError::WrongModule {
            found: *only,
            expected: modifier,
        }),
        other => Err(CustodyError::Modules(other.len())),
    }
}

/// Every address that may act through the modifier (the keeper, the
/// watchdog), with the length of its code: each must have none.
pub fn check_members(members: &[(Address, usize)]) -> Result<(), CustodyError> {
    match members.iter().find(|(_, code)| *code > 0) {
        Some((member, _)) => Err(CustodyError::ContractMember(*member)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runtime code of real Roles modifiers on mainnet, read from the node on
    /// 28 November 2026: a proxy of v2.1.1 and one of v2.1.0.
    const PATCHED: &str =
        "363d3d373d3d3d363d73f2964ce6161ce0e75964fe7927ce114cb0b283d55af43d82803e903d91602b57fd5bf3";
    const VULNERABLE: &str =
        "363d3d373d3d3d363d739646fdad06d3e24444381f44362a3b0eb343d3375af43d82803e903d91602b57fd5bf3";

    #[test]
    fn only_a_proxy_of_the_patched_roles_mastercopy_passes() {
        let modifier = Address::repeat_byte(0x11);
        let patched = hex::decode(PATCHED).unwrap();
        assert_eq!(proxy_target(&patched), Some(ROLES_V2_1_1));
        assert_eq!(check_roles_modifier(modifier, &patched), Ok(()));

        let vulnerable = hex::decode(VULNERABLE).unwrap();
        assert_eq!(proxy_target(&vulnerable), Some(ROLES_V2_1_0));
        let refused = check_roles_modifier(modifier, &vulnerable).unwrap_err();
        assert_eq!(refused, CustodyError::Vulnerable { modifier });
        assert!(refused
            .to_string()
            .contains("deploy a new modifier on v2.1.1"));

        // A proxy of anything else, a Delay, or no proxy at all.
        let mut delay = vulnerable.clone();
        delay[10..30].copy_from_slice(DELAY_V1_1_1.as_slice());
        assert!(matches!(
            check_roles_modifier(modifier, &delay),
            Err(CustodyError::Unknown { target, .. }) if target == DELAY_V1_1_1
        ));
        assert_eq!(
            check_roles_modifier(modifier, &[]),
            Err(CustodyError::NotAProxy(modifier))
        );
        assert_eq!(proxy_target(&patched[..patched.len() - 1]), None);
        let mut altered = patched.clone();
        altered[0] = 0x00;
        assert_eq!(proxy_target(&altered), None);
    }

    #[test]
    fn the_safe_has_the_roles_modifier_and_no_other_module() {
        let modifier = Address::repeat_byte(0x11);
        assert_eq!(check_safe_modules(&[modifier], modifier), Ok(()));
        assert_eq!(
            check_safe_modules(&[], modifier),
            Err(CustodyError::Modules(0))
        );
        assert_eq!(
            check_safe_modules(&[modifier, DELAY_V1_1_0], modifier),
            Err(CustodyError::Modules(2))
        );
        assert!(matches!(
            check_safe_modules(&[DELAY_V1_1_0], modifier),
            Err(CustodyError::WrongModule { found, .. }) if found == DELAY_V1_1_0
        ));
    }

    #[test]
    fn every_member_is_a_plain_account() {
        let keeper = Address::repeat_byte(0x01);
        let watchdog = Address::repeat_byte(0x02);
        assert_eq!(check_members(&[(keeper, 0), (watchdog, 0)]), Ok(()));
        // An account delegated by EIP-7702 has 23 bytes of code.
        assert_eq!(
            check_members(&[(keeper, 0), (watchdog, 23)]),
            Err(CustodyError::ContractMember(watchdog))
        );
    }
}
