pub fn hex_to_bytes<const N: usize>(input: &str) -> Result<[u8; N], &'static str> {
    if input.len() != N * 2 {
        return Err("invalid length");
    }

    let mut out = [0u8; N];
    let bytes = input.as_bytes();
    for (i, item) in out.iter_mut().enumerate().take(N) {
        let hi = bytes[2 * i];
        let lo = bytes[2 * i + 1];
        *item = (hex_val(hi)? << 4) | hex_val(lo)?;
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8, &'static str> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err("invalid hex"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_to_bytes() {
        let input = "0102030405060708090a0b0c0d0e0f";
        let expected: [u8; 15] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        assert_eq!(hex_to_bytes(input).unwrap(), expected);
    }
}
