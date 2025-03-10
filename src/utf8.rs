//! UTF-8 validation and parsing helpers that abstract over [`simdutf8`].
use std::hint::unreachable_unchecked;

use crate::proto::ProtocolError;

/// Converts a slice of bytes to a string slice. This will use SIMD acceleration
/// if available.
///
/// # Errors
///
/// Returns a [`ProtocolError`] if the input is invalid UTF-8.
#[inline]
pub fn parse_str(input: &[u8]) -> Result<&str, ProtocolError> {
    simdutf8::basic::from_utf8(input).map_err(|_| ProtocolError::InvalidUtf8)
}

/// A streaming UTF-8 validator.
#[derive(Debug)]
pub(crate) struct Validator {
    /// Buffer for a partial codepoint. This is four bytes large to copy the
    /// missing bytes into the buffer and reuse the allocation.
    partial_codepoint: [u8; 4],
    /// Length of the partial codepoint currently stored.
    partial_codepoint_len: usize,
}

impl Validator {
    /// Creates a new validator.
    #[cfg(any(feature = "client", feature = "server"))]
    pub fn new() -> Self {
        Self {
            partial_codepoint: [0; 4],
            partial_codepoint_len: 0,
        }
    }

    /// The length of the partial codepoint, once complete.
    #[inline]
    fn complete_codepoint_len(&self) -> usize {
        match self.partial_codepoint[0] {
            // 0b0xxxxxxx (single-byte code point)
            0b0000_0000..=0b0111_1111 => 1,
            // 0b110xxxxx (two-byte code point)
            0b1100_0000..=0b1101_1111 => 2,
            // 0b1110xxxx (three-byte code point)
            0b1110_0000..=0b1110_1111 => 3,
            // 0b11110xxx (four-byte code point)
            0b1111_0000..=0b1111_0111 => 4,
            // Invalid first byte.
            // SAFETY: The first byte must be valid UTF-8, otherwise from_str would return
            // a FromUtf8Error with error_len() that is Some(_)
            _ => unsafe { unreachable_unchecked() },
        }
    }

    /// Resets the validator state.
    #[inline]
    pub fn reset(&mut self) {
        self.partial_codepoint_len = 0;
    }

    /// Feeds bytes into the streaming validator. Returns `Ok` if the input is
    /// valid UTF-8, if `is_complete` is true, even if the input has incomplete
    /// codepoints. Subsequent calls will validate incomplete codepoints
    /// unless [`Self::reset`] is called in between.
    pub fn feed(&mut self, input: &[u8], is_complete: bool) -> Result<(), ProtocolError> {
        // If we have a partial codepoint, complete it
        let remaining_bytes = if self.partial_codepoint_len == 0 {
            input
        } else {
            let available_bytes = input.len();

            if available_bytes == 0 && !is_complete {
                return Ok(());
            }

            let missing_bytes = self.complete_codepoint_len() - self.partial_codepoint_len;
            let bytes_to_copy = available_bytes.min(missing_bytes);
            let codepoint_len_after_copy = self.partial_codepoint_len + bytes_to_copy;

            // Copy the missing codepoint bytes to the partial codepoint
            unsafe {
                self.partial_codepoint
                    .get_unchecked_mut(self.partial_codepoint_len..codepoint_len_after_copy)
            }
            .copy_from_slice(&input[..bytes_to_copy]);

            // If we know that the codepoint is complete, we can use the basic variant
            if available_bytes >= missing_bytes {
                if simdutf8::basic::from_utf8(unsafe {
                    self.partial_codepoint
                        .get_unchecked(..codepoint_len_after_copy)
                })
                .is_err()
                {
                    return Err(ProtocolError::InvalidUtf8);
                }
            } else {
                match simdutf8::compat::from_utf8(unsafe {
                    self.partial_codepoint
                        .get_unchecked(..codepoint_len_after_copy)
                }) {
                    Ok(_) => {}
                    Err(utf8_error) if utf8_error.error_len().is_some() => {
                        return Err(ProtocolError::InvalidUtf8);
                    }
                    Err(_) => {
                        self.partial_codepoint_len = codepoint_len_after_copy;

                        if is_complete {
                            return Err(ProtocolError::InvalidUtf8);
                        }

                        return Ok(());
                    }
                }
            }

            self.reset();

            &input[bytes_to_copy..]
        };

        // Validate the entire rest of the input
        if is_complete {
            self.reset();

            match simdutf8::basic::from_utf8(remaining_bytes) {
                Ok(_) => Ok(()),
                Err(_) => Err(ProtocolError::InvalidUtf8),
            }
        } else {
            match simdutf8::compat::from_utf8(remaining_bytes) {
                Ok(_) => Ok(()),
                Err(utf8_error) if utf8_error.error_len().is_some() => {
                    Err(ProtocolError::InvalidUtf8)
                }
                Err(utf8_error) => {
                    // Incomplete input, copy the partial codepoints to the validator
                    self.partial_codepoint_len = remaining_bytes.len() - utf8_error.valid_up_to();
                    unsafe {
                        self.partial_codepoint
                            .get_unchecked_mut(..self.partial_codepoint_len)
                            .copy_from_slice(
                                remaining_bytes.get_unchecked(utf8_error.valid_up_to()..),
                            );
                    }

                    Ok(())
                }
            }
        }
    }
}
