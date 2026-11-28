//! Solidity ABI encoding, as much of it as the executor's calls need.
//!
//! A call is a four-byte selector, then one 32-byte word per argument in
//! the head, with dynamic arguments (`bytes`) written after the head and
//! pointed at by their offset. A tuple of static types encodes as its
//! fields' words in order, so a function taking one static struct encodes
//! exactly as one taking its fields. Every value is checked against its
//! Solidity type before it is written: a number too large for its type is
//! an error, never a truncation.

use alloy_primitives::{keccak256, Address, B256, U256};

/// One 32-byte ABI word.
pub type Word = [u8; 32];

/// Why a value cannot be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AbiError {
    #[error("{value} does not fit a Solidity {kind}; it must be at most {max}")]
    TooLarge {
        kind: &'static str,
        value: String,
        max: String,
    },
    #[error("{0} does not fit a Solidity int24; it must be between -8388608 and 8388607")]
    Int24(i32),
    #[error("the call data is {len} bytes, too short for {what}; it is not the call expected")]
    Short { what: &'static str, len: usize },
    #[error("the call starts with selector 0x{found}, not 0x{expected}; it is another function")]
    Selector { expected: String, found: String },
    #[error("word {index} holds {found}, which is not a valid {kind}")]
    Invalid {
        index: usize,
        kind: &'static str,
        found: String,
    },
}

/// The four-byte selector of a function: the first four bytes of the
/// keccak-256 of its canonical signature, such as `approve(address,uint256)`.
pub fn selector(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature.as_bytes());
    [hash[0], hash[1], hash[2], hash[3]]
}

/// An address, left-padded with zeros.
pub fn address(value: Address) -> Word {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(value.as_slice());
    word
}

/// A `uint256`.
pub fn uint(value: U256) -> Word {
    value.to_be_bytes::<32>()
}

/// A `uint256` from a machine integer.
pub fn uint_u64(value: u64) -> Word {
    uint(U256::from(value))
}

/// A `uint128`.
pub fn uint128(value: u128) -> Word {
    uint(U256::from(value))
}

/// A number checked to fit `bits` bits, for the `uint<bits>` types.
pub fn uint_bits(value: U256, bits: usize, kind: &'static str) -> Result<Word, AbiError> {
    if value.bit_len() > bits {
        let max = (U256::from(1u8) << bits) - U256::from(1u8);
        return Err(AbiError::TooLarge {
            kind,
            value: value.to_string(),
            max: max.to_string(),
        });
    }
    Ok(uint(value))
}

/// A `uint24`, such as a pool's fee in hundredths of a basis point.
pub fn uint24(value: u32) -> Result<Word, AbiError> {
    uint_bits(U256::from(value), 24, "uint24")
}

/// A `uint160`, such as a square-root price in Q64.96.
pub fn uint160(value: U256) -> Result<Word, AbiError> {
    uint_bits(value, 160, "uint160")
}

/// A `uint8`.
pub fn uint8(value: u8) -> Word {
    uint(U256::from(value))
}

/// An `int24`, such as a tick: two's complement, sign-extended to 32 bytes.
pub fn int24(value: i32) -> Result<Word, AbiError> {
    if !(-(1 << 23)..(1 << 23)).contains(&value) {
        return Err(AbiError::Int24(value));
    }
    let fill = if value < 0 { 0xff } else { 0x00 };
    let mut word = [fill; 32];
    word[28..].copy_from_slice(&value.to_be_bytes());
    Ok(word)
}

/// A `bool`.
pub fn boolean(value: bool) -> Word {
    uint(U256::from(u8::from(value)))
}

/// A `bytes32`.
pub fn bytes32(value: B256) -> Word {
    value.0
}

/// One argument of a call: a static word in the head, or dynamic `bytes`
/// written after the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Word(Word),
    Bytes(Vec<u8>),
}

/// The call data of a function: its selector, then its arguments.
pub fn encode_call(selector: [u8; 4], tokens: &[Token]) -> Vec<u8> {
    let mut head = Vec::with_capacity(32 * tokens.len());
    let mut tail = Vec::new();
    let head_len = 32 * tokens.len();
    for token in tokens {
        match token {
            Token::Word(word) => head.extend_from_slice(word),
            Token::Bytes(data) => {
                head.extend_from_slice(&uint_u64((head_len + tail.len()) as u64));
                tail.extend_from_slice(&uint_u64(data.len() as u64));
                tail.extend_from_slice(data);
                let pad = (32 - data.len() % 32) % 32;
                tail.extend(std::iter::repeat_n(0u8, pad));
            }
        }
    }
    let mut out = Vec::with_capacity(4 + head.len() + tail.len());
    out.extend_from_slice(&selector);
    out.extend_from_slice(&head);
    out.extend_from_slice(&tail);
    out
}

/// Reads the arguments of a call back, word by word.
#[derive(Debug, Clone, Copy)]
pub struct Decoder<'a> {
    args: &'a [u8],
}

impl<'a> Decoder<'a> {
    /// The arguments of `data`, after checking it calls `signature`.
    pub fn new(data: &'a [u8], signature: &str) -> Result<Self, AbiError> {
        let expected = selector(signature);
        if data.len() < 4 {
            return Err(AbiError::Short {
                what: "a selector",
                len: data.len(),
            });
        }
        if data[..4] != expected {
            return Err(AbiError::Selector {
                expected: hex::encode(expected),
                found: hex::encode(&data[..4]),
            });
        }
        Ok(Self { args: &data[4..] })
    }

    /// Argument word `index` of the head.
    pub fn word(&self, index: usize) -> Result<Word, AbiError> {
        let start = 32 * index;
        let slice = self.args.get(start..start + 32).ok_or(AbiError::Short {
            what: "the argument asked for",
            len: self.args.len() + 4,
        })?;
        let mut word = [0u8; 32];
        word.copy_from_slice(slice);
        Ok(word)
    }

    pub fn uint(&self, index: usize) -> Result<U256, AbiError> {
        Ok(U256::from_be_bytes(self.word(index)?))
    }

    /// A word that must fit `bits` bits.
    pub fn uint_bits(&self, index: usize, bits: usize) -> Result<U256, AbiError> {
        let value = self.uint(index)?;
        if value.bit_len() > bits {
            return Err(AbiError::Invalid {
                index,
                kind: "number of that size",
                found: value.to_string(),
            });
        }
        Ok(value)
    }

    pub fn uint128(&self, index: usize) -> Result<u128, AbiError> {
        Ok(self.uint_bits(index, 128)?.to::<u128>())
    }

    pub fn uint24(&self, index: usize) -> Result<u32, AbiError> {
        Ok(self.uint_bits(index, 24)?.to::<u32>())
    }

    pub fn uint8(&self, index: usize) -> Result<u8, AbiError> {
        Ok(self.uint_bits(index, 8)?.to::<u8>())
    }

    pub fn address(&self, index: usize) -> Result<Address, AbiError> {
        let word = self.word(index)?;
        if word[..12].iter().any(|b| *b != 0) {
            return Err(AbiError::Invalid {
                index,
                kind: "address",
                found: hex::encode(word),
            });
        }
        Ok(Address::from_slice(&word[12..]))
    }

    pub fn int24(&self, index: usize) -> Result<i32, AbiError> {
        let word = self.word(index)?;
        let negative = word[28] & 0x80 != 0;
        let fill = if negative { 0xff } else { 0x00 };
        // Sign-extended from 24 bits: the 29 bytes above the low three are
        // all the sign.
        if word[..29].iter().any(|b| *b != fill) {
            return Err(AbiError::Invalid {
                index,
                kind: "int24",
                found: hex::encode(word),
            });
        }
        let mut low = [fill; 4];
        low[1..].copy_from_slice(&word[29..]);
        Ok(i32::from_be_bytes(low))
    }

    pub fn boolean(&self, index: usize) -> Result<bool, AbiError> {
        match self.uint8(index)? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(AbiError::Invalid {
                index,
                kind: "bool",
                found: other.to_string(),
            }),
        }
    }

    pub fn bytes32(&self, index: usize) -> Result<B256, AbiError> {
        Ok(B256::from(self.word(index)?))
    }

    /// Dynamic `bytes` whose offset is head word `index`.
    pub fn bytes(&self, index: usize) -> Result<Vec<u8>, AbiError> {
        let short = AbiError::Short {
            what: "the bytes argument",
            len: self.args.len() + 4,
        };
        let offset = usize::try_from(self.uint(index)?).map_err(|_| short.clone())?;
        let len_word = self
            .args
            .get(offset..offset + 32)
            .ok_or_else(|| short.clone())?;
        let len = usize::try_from(U256::from_be_slice(len_word)).map_err(|_| short.clone())?;
        let start = offset + 32;
        let data = self.args.get(start..start + len).ok_or(short)?;
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn a_selector_is_the_start_of_the_keccak_of_the_signature() {
        // ERC-20 `transfer`, the most widely known selector.
        assert_eq!(
            selector("transfer(address,uint256)"),
            [0xa9, 0x05, 0x9c, 0xbb]
        );
        assert_eq!(
            selector("approve(address,uint256)"),
            [0x09, 0x5e, 0xa7, 0xb3]
        );
    }

    #[test]
    fn words_are_padded_and_signed_as_solidity_pads_and_signs_them() {
        let who = address!("00000000000000000000000000000000000000aa");
        let word = address(who);
        assert_eq!(&word[..12], &[0u8; 12]);
        assert_eq!(word[31], 0xaa);
        assert_eq!(uint_u64(1)[31], 1);
        assert_eq!(boolean(true)[31], 1);
        assert_eq!(boolean(false), [0u8; 32]);
        assert_eq!(uint8(7)[31], 7);

        // A negative tick is two's complement over the whole word.
        let minus_one = int24(-1).unwrap();
        assert_eq!(minus_one, [0xff; 32]);
        let minus = int24(-887_272).unwrap();
        assert!(minus[..29].iter().all(|b| *b == 0xff));
        assert_eq!(&minus[29..], &(-887_272i32).to_be_bytes()[1..]);
        assert_eq!(int24(887_272).unwrap()[29..], 887_272i32.to_be_bytes()[1..]);
        assert!(int24(887_272).unwrap()[..29].iter().all(|b| *b == 0));
        assert_eq!(int24(1 << 23), Err(AbiError::Int24(1 << 23)));
        assert!(int24(-(1 << 23)).is_ok());
        assert!(int24(-(1 << 23) - 1).is_err());

        assert!(uint24(3000).is_ok());
        assert!(uint24(1 << 24).is_err());
        let max160 = (U256::from(1u8) << 160) - U256::from(1u8);
        assert!(uint160(max160).is_ok());
        let refused = uint160(max160 + U256::from(1u8)).unwrap_err().to_string();
        assert!(refused.contains("uint160"), "{refused}");
        assert_eq!(
            bytes32(B256::repeat_byte(0x11)),
            [0x11; 32],
            "a bytes32 is written as it is"
        );
    }

    #[test]
    fn dynamic_bytes_go_after_the_head_at_the_offset_the_head_gives() {
        let data = vec![0xab; 33];
        let encoded = encode_call(
            [1, 2, 3, 4],
            &[
                Token::Word(uint_u64(5)),
                Token::Bytes(data.clone()),
                Token::Word(uint_u64(6)),
            ],
        );
        // Selector, three head words, the length, two words of data.
        assert_eq!(encoded.len(), 4 + 32 * 3 + 32 + 64);
        assert_eq!(&encoded[..4], &[1, 2, 3, 4]);
        let args = &encoded[4..];
        assert_eq!(U256::from_be_slice(&args[32..64]), U256::from(96u8));
        assert_eq!(U256::from_be_slice(&args[96..128]), U256::from(33u8));
        assert_eq!(&args[128..161], &data[..]);
        assert!(args[161..].iter().all(|b| *b == 0), "padded with zeros");

        // Empty bytes: an offset and a zero length, no data words.
        let empty = encode_call([0; 4], &[Token::Bytes(Vec::new())]);
        assert_eq!(empty.len(), 4 + 32 + 32);
    }

    #[test]
    fn the_decoder_reads_back_every_type_and_refuses_what_does_not_fit() {
        let signature = "f(address,uint256,int24,bool,bytes,bytes32,uint24)";
        let who = address!("1111111111111111111111111111111111111111");
        let data = encode_call(
            selector(signature),
            &[
                Token::Word(address(who)),
                Token::Word(uint(U256::from(10u8).pow(U256::from(30u8)))),
                Token::Word(int24(-60).unwrap()),
                Token::Word(boolean(true)),
                Token::Bytes(vec![9, 8, 7]),
                Token::Word(bytes32(B256::repeat_byte(2))),
                Token::Word(uint24(500).unwrap()),
            ],
        );
        let d = Decoder::new(&data, signature).unwrap();
        assert_eq!(d.address(0).unwrap(), who);
        assert_eq!(d.uint(1).unwrap(), U256::from(10u8).pow(U256::from(30u8)));
        assert_eq!(d.int24(2).unwrap(), -60);
        assert!(d.boolean(3).unwrap());
        assert_eq!(d.bytes(4).unwrap(), vec![9, 8, 7]);
        assert_eq!(d.bytes32(5).unwrap(), B256::repeat_byte(2));
        assert_eq!(d.uint24(6).unwrap(), 500);
        // A word with its high bytes set is not an address, an int24, a
        // bool or a uint128; 10^30 fits 128 bits but not 24.
        assert!(d.address(5).is_err());
        assert!(d.boolean(5).is_err());
        assert!(d.int24(5).is_err());
        assert!(d.uint128(5).is_err());
        assert!(d.int24(1).is_err());
        assert!(d.uint128(1).is_ok());
        assert!(d.uint24(1).is_err());
        // Seven head words, then the bytes' length and one data word.
        assert!(d.word(8).is_ok());
        assert!(d.word(9).is_err(), "past the end");

        let other = Decoder::new(&data, "g(uint256)").unwrap_err().to_string();
        assert!(other.contains("another function"), "{other}");
        assert!(Decoder::new(&[1, 2], signature).is_err());
    }
}
