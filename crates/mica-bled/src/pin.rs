//! The PIN rules (protocol section 6).

/// The PIN of a device nobody has claimed: a PIN in the unset state.
pub const DEFAULT: &str = "000000";

/// Six ASCII digits.
pub fn is_valid(pin: &str) -> bool {
    pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit())
}

/// Whether the PIN is still the default.
pub fn is_unset(pin: &str) -> bool {
    pin == DEFAULT
}

/// Whether `pin` may become the device's PIN.
pub fn may_set(pin: &str) -> bool {
    is_valid(pin) && !is_unset(pin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_digits_and_never_the_default() {
        assert!(may_set("123456"));
        assert!(may_set("000001"));
        assert!(!may_set("000000"));
        for bad in [
            "",
            "12345",
            "1234567",
            "12345a",
            "١٢٣٤٥٦",
            " 12345",
            "12 456",
        ] {
            assert!(!may_set(bad), "{bad:?}");
        }
        assert!(is_unset("000000"));
        assert!(!is_unset("123456"));
    }
}
