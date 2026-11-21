/// A rough token count without a tokenizer: about four bytes of ASCII and two characters
/// of other scripts per token. Good enough to split a known total into parts, never a bill.
pub fn estimate_tokens(text: &str) -> u64 {
    let (mut ascii, mut other) = (0u64, 0u64);
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(4) + other.div_ceil(2)
}

#[cfg(test)]
mod tests {
    use super::estimate_tokens;

    #[test]
    fn scripts_count_differently() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("привет"), 3);
    }
}
