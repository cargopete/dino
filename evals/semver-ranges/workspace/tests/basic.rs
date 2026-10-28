use fixture::{Range, Version};

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn yes(range: &str, version: &str) -> bool {
    Range::parse(range).unwrap().matches(&v(version))
}

#[test]
fn versions_parse_and_print() {
    assert_eq!(v("1.2.3").to_string(), "1.2.3");
    assert_eq!(v("v1.2.3-beta.1+exp.5").to_string(), "1.2.3-beta.1+exp.5");
    assert!(Version::parse("1.2").is_err());
    assert!(Version::parse("01.2.3").is_err());
}

#[test]
fn precedence() {
    assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
    assert!(v("1.0.0-beta.2") < v("1.0.0-beta.11"));
    assert!(v("1.0.0-rc.1") < v("1.0.0"));
    assert_eq!(v("1.0.0+a"), v("1.0.0+b"));
}

#[test]
fn simple_ranges() {
    assert!(yes(">=1.2.3 <2.0.0", "1.5.0"));
    assert!(!yes(">=1.2.3 <2.0.0", "2.0.0"));
    assert!(yes("^1.2.3", "1.9.9"));
    assert!(!yes("^0.2.3", "0.3.0"));
    assert!(yes("~1.2.3", "1.2.9"));
    assert!(yes("1.x || >=3", "3.1.0"));
    assert!(yes("1.2.3 - 2.3.4", "2.3.4"));
}

#[test]
fn prereleases_need_an_invitation() {
    assert!(!yes("^1.2.3", "1.3.0-beta"));
    assert!(yes(">=1.2.3-beta", "1.2.3-rc.1"));
    assert!(!yes(">=1.2.3-beta", "1.2.4-rc.1"));
}
