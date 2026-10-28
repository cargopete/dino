//! Generated from node-semver 7.6.3.

use std::cmp::Ordering;

use fixture::{Range, Version};

fn cmp(a: &str, b: &str) -> Ordering {
    Version::parse(a).unwrap().cmp(&Version::parse(b).unwrap())
}

#[test]
fn parse_01() {
    let v = Version::parse("1.2.3").expect("valid");
    assert_eq!(v.to_string(), "1.2.3");
}

#[test]
fn parse_02() {
    let v = Version::parse("v1.2.3").expect("valid");
    assert_eq!(v.to_string(), "1.2.3");
}

#[test]
fn parse_03() {
    let v = Version::parse(" 1.2.3 ").expect("valid");
    assert_eq!(v.to_string(), "1.2.3");
}

#[test]
fn parse_04() {
    let v = Version::parse("0.0.0").expect("valid");
    assert_eq!(v.to_string(), "0.0.0");
}

#[test]
fn parse_05() {
    let v = Version::parse("1.2.3-alpha").expect("valid");
    assert_eq!(v.to_string(), "1.2.3-alpha");
}

#[test]
fn parse_06() {
    let v = Version::parse("1.2.3-alpha.1").expect("valid");
    assert_eq!(v.to_string(), "1.2.3-alpha.1");
}

#[test]
fn parse_07() {
    let v = Version::parse("1.2.3-0.3.7").expect("valid");
    assert_eq!(v.to_string(), "1.2.3-0.3.7");
}

#[test]
fn parse_08() {
    let v = Version::parse("1.2.3-x.7.z.92").expect("valid");
    assert_eq!(v.to_string(), "1.2.3-x.7.z.92");
}

#[test]
fn parse_09() {
    let v = Version::parse("1.2.3+20130313144700").expect("valid");
    assert_eq!(v.to_string(), "1.2.3+20130313144700");
}

#[test]
fn parse_10() {
    let v = Version::parse("1.2.3-beta+exp.sha.5114f85").expect("valid");
    assert_eq!(v.to_string(), "1.2.3-beta+exp.sha.5114f85");
}

#[test]
fn parse_11() {
    let v = Version::parse("1.2.3+001").expect("valid");
    assert_eq!(v.to_string(), "1.2.3+001");
}

#[test]
fn parse_12() {
    let v = Version::parse("1.2.3-x-y-z.--").expect("valid");
    assert_eq!(v.to_string(), "1.2.3-x-y-z.--");
}

#[test]
fn parse_13() {
    assert!(Version::parse("1.2").is_err());
}

#[test]
fn parse_14() {
    assert!(Version::parse("1.2.3.4").is_err());
}

#[test]
fn parse_15() {
    assert!(Version::parse("01.2.3").is_err());
}

#[test]
fn parse_16() {
    assert!(Version::parse("1.02.3").is_err());
}

#[test]
fn parse_17() {
    assert!(Version::parse("1.2.03").is_err());
}

#[test]
fn parse_18() {
    assert!(Version::parse("1.2.3-01").is_err());
}

#[test]
fn parse_19() {
    assert!(Version::parse("1.2.3-").is_err());
}

#[test]
fn parse_20() {
    assert!(Version::parse("1.2.3+").is_err());
}

#[test]
fn parse_21() {
    assert!(Version::parse("1.2.3-a..b").is_err());
}

#[test]
fn parse_22() {
    assert!(Version::parse("1.2.3-a_b").is_err());
}

#[test]
fn parse_23() {
    assert!(Version::parse("a.b.c").is_err());
}

#[test]
fn parse_24() {
    assert!(Version::parse("").is_err());
}

#[test]
fn parse_25() {
    assert!(Version::parse("1.2.3-alpha+").is_err());
}

#[test]
fn parse_26() {
    assert!(Version::parse("=1.2.3").is_err());
}

#[test]
fn parse_27() {
    assert!(Version::parse("V1.2.3").is_err());
}

#[test]
fn parse_28() {
    assert!(Version::parse("1.2.3 4").is_err());
}

#[test]
fn order_01() {
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-alpha"), Ordering::Equal, "1.0.0-alpha vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-alpha.1"), Ordering::Less, "1.0.0-alpha vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-alpha.beta"), Ordering::Less, "1.0.0-alpha vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-beta"), Ordering::Less, "1.0.0-alpha vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-beta.2"), Ordering::Less, "1.0.0-alpha vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-beta.11"), Ordering::Less, "1.0.0-alpha vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-rc.1"), Ordering::Less, "1.0.0-alpha vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0"), Ordering::Less, "1.0.0-alpha vs 1.0.0");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0+build"), Ordering::Less, "1.0.0-alpha vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-alpha", "1.0.1"), Ordering::Less, "1.0.0-alpha vs 1.0.1");
    assert_eq!(cmp("1.0.0-alpha", "1.1.0"), Ordering::Less, "1.0.0-alpha vs 1.1.0");
    assert_eq!(cmp("1.0.0-alpha", "2.0.0"), Ordering::Less, "1.0.0-alpha vs 2.0.0");
    assert_eq!(cmp("1.0.0-alpha", "10.0.0"), Ordering::Less, "1.0.0-alpha vs 10.0.0");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-2"), Ordering::Greater, "1.0.0-alpha vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-10"), Ordering::Greater, "1.0.0-alpha vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-a-b"), Ordering::Greater, "1.0.0-alpha vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-A"), Ordering::Greater, "1.0.0-alpha vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-alpha.1.1"), Ordering::Less, "1.0.0-alpha vs 1.0.0-alpha.1.1");
}

#[test]
fn order_02() {
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-alpha"), Ordering::Greater, "1.0.0-alpha.1 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-alpha.1"), Ordering::Equal, "1.0.0-alpha.1 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-alpha.beta"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-beta"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-beta.2"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-beta.11"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-rc.1"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.0");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0+build"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.1"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.1");
    assert_eq!(cmp("1.0.0-alpha.1", "1.1.0"), Ordering::Less, "1.0.0-alpha.1 vs 1.1.0");
    assert_eq!(cmp("1.0.0-alpha.1", "2.0.0"), Ordering::Less, "1.0.0-alpha.1 vs 2.0.0");
    assert_eq!(cmp("1.0.0-alpha.1", "10.0.0"), Ordering::Less, "1.0.0-alpha.1 vs 10.0.0");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-2"), Ordering::Greater, "1.0.0-alpha.1 vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-10"), Ordering::Greater, "1.0.0-alpha.1 vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-a-b"), Ordering::Greater, "1.0.0-alpha.1 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-A"), Ordering::Greater, "1.0.0-alpha.1 vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-alpha.1.1"), Ordering::Less, "1.0.0-alpha.1 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_03() {
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-alpha"), Ordering::Greater, "1.0.0-alpha.beta vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-alpha.1"), Ordering::Greater, "1.0.0-alpha.beta vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-alpha.beta"), Ordering::Equal, "1.0.0-alpha.beta vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-beta"), Ordering::Less, "1.0.0-alpha.beta vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-beta.2"), Ordering::Less, "1.0.0-alpha.beta vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-beta.11"), Ordering::Less, "1.0.0-alpha.beta vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-rc.1"), Ordering::Less, "1.0.0-alpha.beta vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0"), Ordering::Less, "1.0.0-alpha.beta vs 1.0.0");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0+build"), Ordering::Less, "1.0.0-alpha.beta vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.1"), Ordering::Less, "1.0.0-alpha.beta vs 1.0.1");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.1.0"), Ordering::Less, "1.0.0-alpha.beta vs 1.1.0");
    assert_eq!(cmp("1.0.0-alpha.beta", "2.0.0"), Ordering::Less, "1.0.0-alpha.beta vs 2.0.0");
    assert_eq!(cmp("1.0.0-alpha.beta", "10.0.0"), Ordering::Less, "1.0.0-alpha.beta vs 10.0.0");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-2"), Ordering::Greater, "1.0.0-alpha.beta vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-10"), Ordering::Greater, "1.0.0-alpha.beta vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-a-b"), Ordering::Greater, "1.0.0-alpha.beta vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-A"), Ordering::Greater, "1.0.0-alpha.beta vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-alpha.beta", "1.0.0-alpha.1.1"), Ordering::Greater, "1.0.0-alpha.beta vs 1.0.0-alpha.1.1");
}

#[test]
fn order_04() {
    assert_eq!(cmp("1.0.0-beta", "1.0.0-alpha"), Ordering::Greater, "1.0.0-beta vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-alpha.1"), Ordering::Greater, "1.0.0-beta vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-alpha.beta"), Ordering::Greater, "1.0.0-beta vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-beta"), Ordering::Equal, "1.0.0-beta vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-beta.2"), Ordering::Less, "1.0.0-beta vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-beta.11"), Ordering::Less, "1.0.0-beta vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-rc.1"), Ordering::Less, "1.0.0-beta vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-beta", "1.0.0"), Ordering::Less, "1.0.0-beta vs 1.0.0");
    assert_eq!(cmp("1.0.0-beta", "1.0.0+build"), Ordering::Less, "1.0.0-beta vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-beta", "1.0.1"), Ordering::Less, "1.0.0-beta vs 1.0.1");
    assert_eq!(cmp("1.0.0-beta", "1.1.0"), Ordering::Less, "1.0.0-beta vs 1.1.0");
    assert_eq!(cmp("1.0.0-beta", "2.0.0"), Ordering::Less, "1.0.0-beta vs 2.0.0");
    assert_eq!(cmp("1.0.0-beta", "10.0.0"), Ordering::Less, "1.0.0-beta vs 10.0.0");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-2"), Ordering::Greater, "1.0.0-beta vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-10"), Ordering::Greater, "1.0.0-beta vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-a-b"), Ordering::Greater, "1.0.0-beta vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-A"), Ordering::Greater, "1.0.0-beta vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-beta", "1.0.0-alpha.1.1"), Ordering::Greater, "1.0.0-beta vs 1.0.0-alpha.1.1");
}

#[test]
fn order_05() {
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-alpha"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-alpha.1"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-alpha.beta"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-beta"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-beta.2"), Ordering::Equal, "1.0.0-beta.2 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-beta.11"), Ordering::Less, "1.0.0-beta.2 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-rc.1"), Ordering::Less, "1.0.0-beta.2 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0"), Ordering::Less, "1.0.0-beta.2 vs 1.0.0");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0+build"), Ordering::Less, "1.0.0-beta.2 vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.1"), Ordering::Less, "1.0.0-beta.2 vs 1.0.1");
    assert_eq!(cmp("1.0.0-beta.2", "1.1.0"), Ordering::Less, "1.0.0-beta.2 vs 1.1.0");
    assert_eq!(cmp("1.0.0-beta.2", "2.0.0"), Ordering::Less, "1.0.0-beta.2 vs 2.0.0");
    assert_eq!(cmp("1.0.0-beta.2", "10.0.0"), Ordering::Less, "1.0.0-beta.2 vs 10.0.0");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-2"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-10"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-a-b"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-A"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-beta.2", "1.0.0-alpha.1.1"), Ordering::Greater, "1.0.0-beta.2 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_06() {
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-alpha"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-alpha.1"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-alpha.beta"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-beta"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-beta.2"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-beta.11"), Ordering::Equal, "1.0.0-beta.11 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-rc.1"), Ordering::Less, "1.0.0-beta.11 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0"), Ordering::Less, "1.0.0-beta.11 vs 1.0.0");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0+build"), Ordering::Less, "1.0.0-beta.11 vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.1"), Ordering::Less, "1.0.0-beta.11 vs 1.0.1");
    assert_eq!(cmp("1.0.0-beta.11", "1.1.0"), Ordering::Less, "1.0.0-beta.11 vs 1.1.0");
    assert_eq!(cmp("1.0.0-beta.11", "2.0.0"), Ordering::Less, "1.0.0-beta.11 vs 2.0.0");
    assert_eq!(cmp("1.0.0-beta.11", "10.0.0"), Ordering::Less, "1.0.0-beta.11 vs 10.0.0");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-2"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-10"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-a-b"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-A"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-alpha.1.1"), Ordering::Greater, "1.0.0-beta.11 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_07() {
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-alpha"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-alpha.1"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-alpha.beta"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-beta"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-beta.2"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-beta.11"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-rc.1"), Ordering::Equal, "1.0.0-rc.1 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0"), Ordering::Less, "1.0.0-rc.1 vs 1.0.0");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0+build"), Ordering::Less, "1.0.0-rc.1 vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.1"), Ordering::Less, "1.0.0-rc.1 vs 1.0.1");
    assert_eq!(cmp("1.0.0-rc.1", "1.1.0"), Ordering::Less, "1.0.0-rc.1 vs 1.1.0");
    assert_eq!(cmp("1.0.0-rc.1", "2.0.0"), Ordering::Less, "1.0.0-rc.1 vs 2.0.0");
    assert_eq!(cmp("1.0.0-rc.1", "10.0.0"), Ordering::Less, "1.0.0-rc.1 vs 10.0.0");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-2"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-10"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-a-b"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-A"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-rc.1", "1.0.0-alpha.1.1"), Ordering::Greater, "1.0.0-rc.1 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_08() {
    assert_eq!(cmp("1.0.0", "1.0.0-alpha"), Ordering::Greater, "1.0.0 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0", "1.0.0-alpha.1"), Ordering::Greater, "1.0.0 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0", "1.0.0-alpha.beta"), Ordering::Greater, "1.0.0 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0", "1.0.0-beta"), Ordering::Greater, "1.0.0 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0", "1.0.0-beta.2"), Ordering::Greater, "1.0.0 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0", "1.0.0-beta.11"), Ordering::Greater, "1.0.0 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0", "1.0.0-rc.1"), Ordering::Greater, "1.0.0 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0", "1.0.0"), Ordering::Equal, "1.0.0 vs 1.0.0");
    assert_eq!(cmp("1.0.0", "1.0.0+build"), Ordering::Equal, "1.0.0 vs 1.0.0+build");
    assert_eq!(cmp("1.0.0", "1.0.1"), Ordering::Less, "1.0.0 vs 1.0.1");
    assert_eq!(cmp("1.0.0", "1.1.0"), Ordering::Less, "1.0.0 vs 1.1.0");
    assert_eq!(cmp("1.0.0", "2.0.0"), Ordering::Less, "1.0.0 vs 2.0.0");
    assert_eq!(cmp("1.0.0", "10.0.0"), Ordering::Less, "1.0.0 vs 10.0.0");
    assert_eq!(cmp("1.0.0", "1.0.0-2"), Ordering::Greater, "1.0.0 vs 1.0.0-2");
    assert_eq!(cmp("1.0.0", "1.0.0-10"), Ordering::Greater, "1.0.0 vs 1.0.0-10");
    assert_eq!(cmp("1.0.0", "1.0.0-a-b"), Ordering::Greater, "1.0.0 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0", "1.0.0-A"), Ordering::Greater, "1.0.0 vs 1.0.0-A");
    assert_eq!(cmp("1.0.0", "1.0.0-alpha.1.1"), Ordering::Greater, "1.0.0 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_09() {
    assert_eq!(cmp("1.0.0+build", "1.0.0-alpha"), Ordering::Greater, "1.0.0+build vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0+build", "1.0.0-alpha.1"), Ordering::Greater, "1.0.0+build vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0+build", "1.0.0-alpha.beta"), Ordering::Greater, "1.0.0+build vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0+build", "1.0.0-beta"), Ordering::Greater, "1.0.0+build vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0+build", "1.0.0-beta.2"), Ordering::Greater, "1.0.0+build vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0+build", "1.0.0-beta.11"), Ordering::Greater, "1.0.0+build vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0+build", "1.0.0-rc.1"), Ordering::Greater, "1.0.0+build vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0+build", "1.0.0"), Ordering::Equal, "1.0.0+build vs 1.0.0");
    assert_eq!(cmp("1.0.0+build", "1.0.0+build"), Ordering::Equal, "1.0.0+build vs 1.0.0+build");
    assert_eq!(cmp("1.0.0+build", "1.0.1"), Ordering::Less, "1.0.0+build vs 1.0.1");
    assert_eq!(cmp("1.0.0+build", "1.1.0"), Ordering::Less, "1.0.0+build vs 1.1.0");
    assert_eq!(cmp("1.0.0+build", "2.0.0"), Ordering::Less, "1.0.0+build vs 2.0.0");
    assert_eq!(cmp("1.0.0+build", "10.0.0"), Ordering::Less, "1.0.0+build vs 10.0.0");
    assert_eq!(cmp("1.0.0+build", "1.0.0-2"), Ordering::Greater, "1.0.0+build vs 1.0.0-2");
    assert_eq!(cmp("1.0.0+build", "1.0.0-10"), Ordering::Greater, "1.0.0+build vs 1.0.0-10");
    assert_eq!(cmp("1.0.0+build", "1.0.0-a-b"), Ordering::Greater, "1.0.0+build vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0+build", "1.0.0-A"), Ordering::Greater, "1.0.0+build vs 1.0.0-A");
    assert_eq!(cmp("1.0.0+build", "1.0.0-alpha.1.1"), Ordering::Greater, "1.0.0+build vs 1.0.0-alpha.1.1");
}

#[test]
fn order_10() {
    assert_eq!(cmp("1.0.1", "1.0.0-alpha"), Ordering::Greater, "1.0.1 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.1", "1.0.0-alpha.1"), Ordering::Greater, "1.0.1 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.1", "1.0.0-alpha.beta"), Ordering::Greater, "1.0.1 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.1", "1.0.0-beta"), Ordering::Greater, "1.0.1 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.1", "1.0.0-beta.2"), Ordering::Greater, "1.0.1 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.1", "1.0.0-beta.11"), Ordering::Greater, "1.0.1 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.1", "1.0.0-rc.1"), Ordering::Greater, "1.0.1 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.1", "1.0.0"), Ordering::Greater, "1.0.1 vs 1.0.0");
    assert_eq!(cmp("1.0.1", "1.0.0+build"), Ordering::Greater, "1.0.1 vs 1.0.0+build");
    assert_eq!(cmp("1.0.1", "1.0.1"), Ordering::Equal, "1.0.1 vs 1.0.1");
    assert_eq!(cmp("1.0.1", "1.1.0"), Ordering::Less, "1.0.1 vs 1.1.0");
    assert_eq!(cmp("1.0.1", "2.0.0"), Ordering::Less, "1.0.1 vs 2.0.0");
    assert_eq!(cmp("1.0.1", "10.0.0"), Ordering::Less, "1.0.1 vs 10.0.0");
    assert_eq!(cmp("1.0.1", "1.0.0-2"), Ordering::Greater, "1.0.1 vs 1.0.0-2");
    assert_eq!(cmp("1.0.1", "1.0.0-10"), Ordering::Greater, "1.0.1 vs 1.0.0-10");
    assert_eq!(cmp("1.0.1", "1.0.0-a-b"), Ordering::Greater, "1.0.1 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.1", "1.0.0-A"), Ordering::Greater, "1.0.1 vs 1.0.0-A");
    assert_eq!(cmp("1.0.1", "1.0.0-alpha.1.1"), Ordering::Greater, "1.0.1 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_11() {
    assert_eq!(cmp("1.1.0", "1.0.0-alpha"), Ordering::Greater, "1.1.0 vs 1.0.0-alpha");
    assert_eq!(cmp("1.1.0", "1.0.0-alpha.1"), Ordering::Greater, "1.1.0 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.1.0", "1.0.0-alpha.beta"), Ordering::Greater, "1.1.0 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.1.0", "1.0.0-beta"), Ordering::Greater, "1.1.0 vs 1.0.0-beta");
    assert_eq!(cmp("1.1.0", "1.0.0-beta.2"), Ordering::Greater, "1.1.0 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.1.0", "1.0.0-beta.11"), Ordering::Greater, "1.1.0 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.1.0", "1.0.0-rc.1"), Ordering::Greater, "1.1.0 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.1.0", "1.0.0"), Ordering::Greater, "1.1.0 vs 1.0.0");
    assert_eq!(cmp("1.1.0", "1.0.0+build"), Ordering::Greater, "1.1.0 vs 1.0.0+build");
    assert_eq!(cmp("1.1.0", "1.0.1"), Ordering::Greater, "1.1.0 vs 1.0.1");
    assert_eq!(cmp("1.1.0", "1.1.0"), Ordering::Equal, "1.1.0 vs 1.1.0");
    assert_eq!(cmp("1.1.0", "2.0.0"), Ordering::Less, "1.1.0 vs 2.0.0");
    assert_eq!(cmp("1.1.0", "10.0.0"), Ordering::Less, "1.1.0 vs 10.0.0");
    assert_eq!(cmp("1.1.0", "1.0.0-2"), Ordering::Greater, "1.1.0 vs 1.0.0-2");
    assert_eq!(cmp("1.1.0", "1.0.0-10"), Ordering::Greater, "1.1.0 vs 1.0.0-10");
    assert_eq!(cmp("1.1.0", "1.0.0-a-b"), Ordering::Greater, "1.1.0 vs 1.0.0-a-b");
    assert_eq!(cmp("1.1.0", "1.0.0-A"), Ordering::Greater, "1.1.0 vs 1.0.0-A");
    assert_eq!(cmp("1.1.0", "1.0.0-alpha.1.1"), Ordering::Greater, "1.1.0 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_12() {
    assert_eq!(cmp("2.0.0", "1.0.0-alpha"), Ordering::Greater, "2.0.0 vs 1.0.0-alpha");
    assert_eq!(cmp("2.0.0", "1.0.0-alpha.1"), Ordering::Greater, "2.0.0 vs 1.0.0-alpha.1");
    assert_eq!(cmp("2.0.0", "1.0.0-alpha.beta"), Ordering::Greater, "2.0.0 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("2.0.0", "1.0.0-beta"), Ordering::Greater, "2.0.0 vs 1.0.0-beta");
    assert_eq!(cmp("2.0.0", "1.0.0-beta.2"), Ordering::Greater, "2.0.0 vs 1.0.0-beta.2");
    assert_eq!(cmp("2.0.0", "1.0.0-beta.11"), Ordering::Greater, "2.0.0 vs 1.0.0-beta.11");
    assert_eq!(cmp("2.0.0", "1.0.0-rc.1"), Ordering::Greater, "2.0.0 vs 1.0.0-rc.1");
    assert_eq!(cmp("2.0.0", "1.0.0"), Ordering::Greater, "2.0.0 vs 1.0.0");
    assert_eq!(cmp("2.0.0", "1.0.0+build"), Ordering::Greater, "2.0.0 vs 1.0.0+build");
    assert_eq!(cmp("2.0.0", "1.0.1"), Ordering::Greater, "2.0.0 vs 1.0.1");
    assert_eq!(cmp("2.0.0", "1.1.0"), Ordering::Greater, "2.0.0 vs 1.1.0");
    assert_eq!(cmp("2.0.0", "2.0.0"), Ordering::Equal, "2.0.0 vs 2.0.0");
    assert_eq!(cmp("2.0.0", "10.0.0"), Ordering::Less, "2.0.0 vs 10.0.0");
    assert_eq!(cmp("2.0.0", "1.0.0-2"), Ordering::Greater, "2.0.0 vs 1.0.0-2");
    assert_eq!(cmp("2.0.0", "1.0.0-10"), Ordering::Greater, "2.0.0 vs 1.0.0-10");
    assert_eq!(cmp("2.0.0", "1.0.0-a-b"), Ordering::Greater, "2.0.0 vs 1.0.0-a-b");
    assert_eq!(cmp("2.0.0", "1.0.0-A"), Ordering::Greater, "2.0.0 vs 1.0.0-A");
    assert_eq!(cmp("2.0.0", "1.0.0-alpha.1.1"), Ordering::Greater, "2.0.0 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_13() {
    assert_eq!(cmp("10.0.0", "1.0.0-alpha"), Ordering::Greater, "10.0.0 vs 1.0.0-alpha");
    assert_eq!(cmp("10.0.0", "1.0.0-alpha.1"), Ordering::Greater, "10.0.0 vs 1.0.0-alpha.1");
    assert_eq!(cmp("10.0.0", "1.0.0-alpha.beta"), Ordering::Greater, "10.0.0 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("10.0.0", "1.0.0-beta"), Ordering::Greater, "10.0.0 vs 1.0.0-beta");
    assert_eq!(cmp("10.0.0", "1.0.0-beta.2"), Ordering::Greater, "10.0.0 vs 1.0.0-beta.2");
    assert_eq!(cmp("10.0.0", "1.0.0-beta.11"), Ordering::Greater, "10.0.0 vs 1.0.0-beta.11");
    assert_eq!(cmp("10.0.0", "1.0.0-rc.1"), Ordering::Greater, "10.0.0 vs 1.0.0-rc.1");
    assert_eq!(cmp("10.0.0", "1.0.0"), Ordering::Greater, "10.0.0 vs 1.0.0");
    assert_eq!(cmp("10.0.0", "1.0.0+build"), Ordering::Greater, "10.0.0 vs 1.0.0+build");
    assert_eq!(cmp("10.0.0", "1.0.1"), Ordering::Greater, "10.0.0 vs 1.0.1");
    assert_eq!(cmp("10.0.0", "1.1.0"), Ordering::Greater, "10.0.0 vs 1.1.0");
    assert_eq!(cmp("10.0.0", "2.0.0"), Ordering::Greater, "10.0.0 vs 2.0.0");
    assert_eq!(cmp("10.0.0", "10.0.0"), Ordering::Equal, "10.0.0 vs 10.0.0");
    assert_eq!(cmp("10.0.0", "1.0.0-2"), Ordering::Greater, "10.0.0 vs 1.0.0-2");
    assert_eq!(cmp("10.0.0", "1.0.0-10"), Ordering::Greater, "10.0.0 vs 1.0.0-10");
    assert_eq!(cmp("10.0.0", "1.0.0-a-b"), Ordering::Greater, "10.0.0 vs 1.0.0-a-b");
    assert_eq!(cmp("10.0.0", "1.0.0-A"), Ordering::Greater, "10.0.0 vs 1.0.0-A");
    assert_eq!(cmp("10.0.0", "1.0.0-alpha.1.1"), Ordering::Greater, "10.0.0 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_14() {
    assert_eq!(cmp("1.0.0-2", "1.0.0-alpha"), Ordering::Less, "1.0.0-2 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-2", "1.0.0-alpha.1"), Ordering::Less, "1.0.0-2 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-2", "1.0.0-alpha.beta"), Ordering::Less, "1.0.0-2 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-2", "1.0.0-beta"), Ordering::Less, "1.0.0-2 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-2", "1.0.0-beta.2"), Ordering::Less, "1.0.0-2 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-2", "1.0.0-beta.11"), Ordering::Less, "1.0.0-2 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-2", "1.0.0-rc.1"), Ordering::Less, "1.0.0-2 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-2", "1.0.0"), Ordering::Less, "1.0.0-2 vs 1.0.0");
    assert_eq!(cmp("1.0.0-2", "1.0.0+build"), Ordering::Less, "1.0.0-2 vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-2", "1.0.1"), Ordering::Less, "1.0.0-2 vs 1.0.1");
    assert_eq!(cmp("1.0.0-2", "1.1.0"), Ordering::Less, "1.0.0-2 vs 1.1.0");
    assert_eq!(cmp("1.0.0-2", "2.0.0"), Ordering::Less, "1.0.0-2 vs 2.0.0");
    assert_eq!(cmp("1.0.0-2", "10.0.0"), Ordering::Less, "1.0.0-2 vs 10.0.0");
    assert_eq!(cmp("1.0.0-2", "1.0.0-2"), Ordering::Equal, "1.0.0-2 vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-2", "1.0.0-10"), Ordering::Less, "1.0.0-2 vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-2", "1.0.0-a-b"), Ordering::Less, "1.0.0-2 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-2", "1.0.0-A"), Ordering::Less, "1.0.0-2 vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-2", "1.0.0-alpha.1.1"), Ordering::Less, "1.0.0-2 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_15() {
    assert_eq!(cmp("1.0.0-10", "1.0.0-alpha"), Ordering::Less, "1.0.0-10 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-10", "1.0.0-alpha.1"), Ordering::Less, "1.0.0-10 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-10", "1.0.0-alpha.beta"), Ordering::Less, "1.0.0-10 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-10", "1.0.0-beta"), Ordering::Less, "1.0.0-10 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-10", "1.0.0-beta.2"), Ordering::Less, "1.0.0-10 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-10", "1.0.0-beta.11"), Ordering::Less, "1.0.0-10 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-10", "1.0.0-rc.1"), Ordering::Less, "1.0.0-10 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-10", "1.0.0"), Ordering::Less, "1.0.0-10 vs 1.0.0");
    assert_eq!(cmp("1.0.0-10", "1.0.0+build"), Ordering::Less, "1.0.0-10 vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-10", "1.0.1"), Ordering::Less, "1.0.0-10 vs 1.0.1");
    assert_eq!(cmp("1.0.0-10", "1.1.0"), Ordering::Less, "1.0.0-10 vs 1.1.0");
    assert_eq!(cmp("1.0.0-10", "2.0.0"), Ordering::Less, "1.0.0-10 vs 2.0.0");
    assert_eq!(cmp("1.0.0-10", "10.0.0"), Ordering::Less, "1.0.0-10 vs 10.0.0");
    assert_eq!(cmp("1.0.0-10", "1.0.0-2"), Ordering::Greater, "1.0.0-10 vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-10", "1.0.0-10"), Ordering::Equal, "1.0.0-10 vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-10", "1.0.0-a-b"), Ordering::Less, "1.0.0-10 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-10", "1.0.0-A"), Ordering::Less, "1.0.0-10 vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-10", "1.0.0-alpha.1.1"), Ordering::Less, "1.0.0-10 vs 1.0.0-alpha.1.1");
}

#[test]
fn order_16() {
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-alpha"), Ordering::Less, "1.0.0-a-b vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-alpha.1"), Ordering::Less, "1.0.0-a-b vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-alpha.beta"), Ordering::Less, "1.0.0-a-b vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-beta"), Ordering::Less, "1.0.0-a-b vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-beta.2"), Ordering::Less, "1.0.0-a-b vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-beta.11"), Ordering::Less, "1.0.0-a-b vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-rc.1"), Ordering::Less, "1.0.0-a-b vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0"), Ordering::Less, "1.0.0-a-b vs 1.0.0");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0+build"), Ordering::Less, "1.0.0-a-b vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-a-b", "1.0.1"), Ordering::Less, "1.0.0-a-b vs 1.0.1");
    assert_eq!(cmp("1.0.0-a-b", "1.1.0"), Ordering::Less, "1.0.0-a-b vs 1.1.0");
    assert_eq!(cmp("1.0.0-a-b", "2.0.0"), Ordering::Less, "1.0.0-a-b vs 2.0.0");
    assert_eq!(cmp("1.0.0-a-b", "10.0.0"), Ordering::Less, "1.0.0-a-b vs 10.0.0");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-2"), Ordering::Greater, "1.0.0-a-b vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-10"), Ordering::Greater, "1.0.0-a-b vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-a-b"), Ordering::Equal, "1.0.0-a-b vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-A"), Ordering::Greater, "1.0.0-a-b vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-a-b", "1.0.0-alpha.1.1"), Ordering::Less, "1.0.0-a-b vs 1.0.0-alpha.1.1");
}

#[test]
fn order_17() {
    assert_eq!(cmp("1.0.0-A", "1.0.0-alpha"), Ordering::Less, "1.0.0-A vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-A", "1.0.0-alpha.1"), Ordering::Less, "1.0.0-A vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-A", "1.0.0-alpha.beta"), Ordering::Less, "1.0.0-A vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-A", "1.0.0-beta"), Ordering::Less, "1.0.0-A vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-A", "1.0.0-beta.2"), Ordering::Less, "1.0.0-A vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-A", "1.0.0-beta.11"), Ordering::Less, "1.0.0-A vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-A", "1.0.0-rc.1"), Ordering::Less, "1.0.0-A vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-A", "1.0.0"), Ordering::Less, "1.0.0-A vs 1.0.0");
    assert_eq!(cmp("1.0.0-A", "1.0.0+build"), Ordering::Less, "1.0.0-A vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-A", "1.0.1"), Ordering::Less, "1.0.0-A vs 1.0.1");
    assert_eq!(cmp("1.0.0-A", "1.1.0"), Ordering::Less, "1.0.0-A vs 1.1.0");
    assert_eq!(cmp("1.0.0-A", "2.0.0"), Ordering::Less, "1.0.0-A vs 2.0.0");
    assert_eq!(cmp("1.0.0-A", "10.0.0"), Ordering::Less, "1.0.0-A vs 10.0.0");
    assert_eq!(cmp("1.0.0-A", "1.0.0-2"), Ordering::Greater, "1.0.0-A vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-A", "1.0.0-10"), Ordering::Greater, "1.0.0-A vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-A", "1.0.0-a-b"), Ordering::Less, "1.0.0-A vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-A", "1.0.0-A"), Ordering::Equal, "1.0.0-A vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-A", "1.0.0-alpha.1.1"), Ordering::Less, "1.0.0-A vs 1.0.0-alpha.1.1");
}

#[test]
fn order_18() {
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-alpha"), Ordering::Greater, "1.0.0-alpha.1.1 vs 1.0.0-alpha");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-alpha.1"), Ordering::Greater, "1.0.0-alpha.1.1 vs 1.0.0-alpha.1");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-alpha.beta"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.0.0-alpha.beta");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-beta"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.0.0-beta");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-beta.2"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.0.0-beta.2");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-beta.11"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.0.0-beta.11");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-rc.1"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.0.0-rc.1");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.0.0");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0+build"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.0.0+build");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.1"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.0.1");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.1.0"), Ordering::Less, "1.0.0-alpha.1.1 vs 1.1.0");
    assert_eq!(cmp("1.0.0-alpha.1.1", "2.0.0"), Ordering::Less, "1.0.0-alpha.1.1 vs 2.0.0");
    assert_eq!(cmp("1.0.0-alpha.1.1", "10.0.0"), Ordering::Less, "1.0.0-alpha.1.1 vs 10.0.0");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-2"), Ordering::Greater, "1.0.0-alpha.1.1 vs 1.0.0-2");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-10"), Ordering::Greater, "1.0.0-alpha.1.1 vs 1.0.0-10");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-a-b"), Ordering::Greater, "1.0.0-alpha.1.1 vs 1.0.0-a-b");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-A"), Ordering::Greater, "1.0.0-alpha.1.1 vs 1.0.0-A");
    assert_eq!(cmp("1.0.0-alpha.1.1", "1.0.0-alpha.1.1"), Ordering::Equal, "1.0.0-alpha.1.1 vs 1.0.0-alpha.1.1");
}

#[test]
fn satisfying_01() {
    let versions: Vec<Version> = ["1.2.3", "1.9.9", "2.0.0", "1.10.0", "1.2.4-beta"].iter().map(|v| Version::parse(v).unwrap()).collect();
    let range = Range::parse("^1.2.3").unwrap();
    assert_eq!(range.max_satisfying(&versions).map(ToString::to_string), Some("1.10.0".to_owned()));
    assert_eq!(range.min_satisfying(&versions).map(ToString::to_string), Some("1.2.3".to_owned()));
}

#[test]
fn satisfying_02() {
    let versions: Vec<Version> = ["1.2.0", "1.2.10", "1.2.9", "1.3.0"].iter().map(|v| Version::parse(v).unwrap()).collect();
    let range = Range::parse("~1.2").unwrap();
    assert_eq!(range.max_satisfying(&versions).map(ToString::to_string), Some("1.2.10".to_owned()));
    assert_eq!(range.min_satisfying(&versions).map(ToString::to_string), Some("1.2.0".to_owned()));
}

#[test]
fn satisfying_03() {
    let versions: Vec<Version> = ["1.0.0", "1.5.0"].iter().map(|v| Version::parse(v).unwrap()).collect();
    let range = Range::parse(">=2.0.0").unwrap();
    assert_eq!(range.max_satisfying(&versions).map(ToString::to_string), None);
    assert_eq!(range.min_satisfying(&versions).map(ToString::to_string), None);
}

#[test]
fn satisfying_04() {
    let versions: Vec<Version> = ["1.0.0+a", "1.0.0+b", "0.9.0"].iter().map(|v| Version::parse(v).unwrap()).collect();
    let range = Range::parse("*").unwrap();
    assert_eq!(range.max_satisfying(&versions).map(ToString::to_string), Some("1.0.0+a".to_owned()));
    assert_eq!(range.min_satisfying(&versions).map(ToString::to_string), Some("0.9.0".to_owned()));
}

#[test]
fn satisfying_05() {
    let versions: Vec<Version> = ["1.2.3-alpha", "1.2.3-beta.3", "1.2.3-rc.1", "1.2.3"].iter().map(|v| Version::parse(v).unwrap()).collect();
    let range = Range::parse("1.2.3-beta - 1.2.3").unwrap();
    assert_eq!(range.max_satisfying(&versions).map(ToString::to_string), Some("1.2.3".to_owned()));
    assert_eq!(range.min_satisfying(&versions).map(ToString::to_string), Some("1.2.3-beta.3".to_owned()));
}

