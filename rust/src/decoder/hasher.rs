//! Verihash message hasher.
//!
//! WARNING: this is an experimental PoC-quality implementation!
//! It is NOT suitable for production use!

// TODO(tarcieri): tests and test vectors!!!

use crate::{
    decoder::Event,
    error::Error,
    field::{self, Tag, WireType},
};
use core::fmt::{self, Debug};
use digest::Digest;

/// Verihash prefix used by tags (unsigned integer)
const TAG_PREFIX: u8 = WireType::UInt64.to_u8();

/// Verihash message hasher.
///
/// This type computes a hash-based transcript of how a message was
/// decoded, driven by incoming decoding events.
pub struct Hasher<D: Digest> {
    /// Computed digest in-progress
    digest: D,

    /// Current state of the decoder (or `None` if an error occurred)
    state: Option<State>,
}

impl<D> Hasher<D>
where
    D: Digest,
{
    /// Create a new [`Hasher`]
    pub fn new() -> Self {
        Self::default()
    }

    /// Hash an incoming event
    pub fn hash_event(&mut self, event: &Event<'_>) -> Result<(), Error> {
        if let Some(state) = self.state.take() {
            let new_state = state.transition(event, &mut self.digest)?;
            self.state = Some(new_state);
            Ok(())
        } else {
            Err(Error::Failed)
        }
    }
}

impl<D> Default for Hasher<D>
where
    D: Digest,
{
    fn default() -> Self {
        Self {
            digest: D::new(),
            state: Some(State::default()),
        }
    }
}

impl<D> Debug for Hasher<D>
where
    D: Digest,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hasher").finish()
    }
}

#[derive(Debug, Eq, PartialEq)]
enum State {
    /// At the start of a message with no data processed
    Initial,

    /// Field header has been read
    Header(field::Header),

    /// Hashing a bytes field
    Bytes { remaining: usize },

    /// Hashing a string field
    String { remaining: usize },

    /// Hashing a message value
    Message { remaining: usize },

    /// Hashing a sequence value
    Sequence { remaining: usize },
}

impl Default for State {
    fn default() -> Self {
        State::Initial
    }
}

impl State {
    /// Transition to a new state based on an incoming event or return an error
    pub fn transition<D: Digest>(self, event: &Event<'_>, digest: &mut D) -> Result<Self, Error> {
        match event {
            Event::FieldHeader(header) => self.handle_field_header(header),
            Event::LengthDelimiter { wire_type, length } => {
                self.handle_length_delimiter(*wire_type, *length, digest)
            }
            Event::Bool(_) | Event::UInt64(_) | Event::SInt64(_) => {
                self.handle_fixed_sized_value(event, digest)
            }
            Event::ValueChunk {
                wire_type,
                bytes,
                remaining,
            } => self.handle_value_chunk(*wire_type, bytes, *remaining, digest),
            Event::SequenceHeader { wire_type, length } => {
                self.handle_sequence_header(*wire_type, *length)
            }
        }
    }

    /// Handle an incoming field header
    fn handle_field_header(self, header: &field::Header) -> Result<Self, Error> {
        if self == State::Initial {
            Ok(State::Header(*header))
        } else {
            Err(Error::Hashing)
        }
    }

    /// Handle length delimiter event
    fn handle_length_delimiter<D: Digest>(
        self,
        wire_type: WireType,
        length: usize,
        digest: &mut D,
    ) -> Result<Self, Error> {
        if let State::Header(header) = self {
            if wire_type != header.wire_type {
                return Err(Error::Hashing);
            }

            let new_state = match wire_type {
                WireType::Bytes => State::Bytes { remaining: length },
                WireType::String => State::String { remaining: length },
                WireType::Message => State::Message { remaining: length },
                WireType::Sequence => State::Sequence { remaining: length },
                _ => unreachable!(),
            };

            hash_dynamically_sized_value(digest, header.tag, wire_type, length);

            Ok(new_state)
        } else {
            Err(Error::Hashing)
        }
    }

    /// Handle hashing an incoming fixed-width value
    fn handle_fixed_sized_value<D: Digest>(
        self,
        value: &Event<'_>,
        digest: &mut D,
    ) -> Result<Self, Error> {
        if let State::Header(header) = self {
            match value {
                Event::Bool(value) => hash_boolean(digest, header.tag, *value),
                Event::UInt64(value) => hash_uint64(digest, header.tag, *value),
                Event::SInt64(value) => hash_sint64(digest, header.tag, *value),
                _ => unreachable!(),
            }
        } else {
            return Err(Error::Hashing);
        }

        Ok(State::Initial)
    }

    /// Handle an incoming chunk of data in a value
    fn handle_value_chunk<D: Digest>(
        self,
        wire_type: WireType,
        bytes: &[u8],
        new_remaining: usize,
        digest: &mut D,
    ) -> Result<Self, Error> {
        // TODO(tarcieri): DRY this out
        let new_state = match self {
            State::Bytes { remaining } => {
                if wire_type != WireType::Bytes || remaining - bytes.len() != new_remaining {
                    return Err(Error::Hashing);
                }

                if new_remaining == 0 {
                    State::Initial
                } else {
                    State::Bytes {
                        remaining: new_remaining,
                    }
                }
            }
            State::String { remaining } => {
                // TODO(tarcieri): use `unicode-normalization`?

                if wire_type != WireType::String || remaining - bytes.len() != new_remaining {
                    return Err(Error::Hashing);
                }

                if new_remaining == 0 {
                    State::Initial
                } else {
                    State::String {
                        remaining: new_remaining,
                    }
                }
            }
            State::Message { remaining } => {
                if wire_type != WireType::Message || remaining - bytes.len() != new_remaining {
                    return Err(Error::Hashing);
                }

                if new_remaining == 0 {
                    return Ok(State::Initial);
                } else {
                    return Ok(State::Bytes {
                        remaining: new_remaining,
                    });
                }
            }
            State::Sequence { remaining } => {
                if wire_type != WireType::Sequence || remaining - bytes.len() != new_remaining {
                    return Err(Error::Hashing);
                }

                if new_remaining == 0 {
                    return Ok(State::Initial);
                } else {
                    return Ok(State::Bytes {
                        remaining: new_remaining,
                    });
                }
            }
            _ => {
                return Err(Error::Hashing);
            }
        };

        digest.input(bytes);
        Ok(new_state)
    }

    /// Handle an incoming sequence header
    fn handle_sequence_header(self, _wire_type: WireType, _length: usize) -> Result<Self, Error> {
        // TODO(tarcieri): handle sequence headers correctly!
        Ok(self)
    }
}

/// Hash a boolean
pub fn hash_boolean<D: Digest>(digest: &mut D, tag: Tag, value: bool) {
    let (wire_type, body) = if value {
        (WireType::True, b"\x01")
    } else {
        (WireType::False, b"\x00")
    };

    hash_fixed(digest, tag, wire_type, body);
}

/// Hash an unsigned integer
pub fn hash_uint64<D: Digest>(digest: &mut D, tag: Tag, value: u64) {
    hash_fixed(digest, tag, WireType::UInt64, &value.to_le_bytes());
}

/// Hash a signed integer
pub fn hash_sint64<D: Digest>(digest: &mut D, tag: Tag, value: i64) {
    hash_fixed(digest, tag, WireType::SInt64, &value.to_le_bytes());
}

/// Hash bytes
pub fn hash_dynamically_sized_value<D: Digest>(
    digest: &mut D,
    tag: Tag,
    wire_type: WireType,
    length: usize,
) {
    digest.input(&[TAG_PREFIX]);
    digest.input(&tag.to_le_bytes());
    digest.input(&[wire_type.to_u8()]);
    digest.input(&(length as u64).to_le_bytes());
}

/// Hash a fixed-width value with the given wiretype
fn hash_fixed<D: Digest>(digest: &mut D, tag: Tag, wire_type: WireType, body: &[u8]) {
    digest.input(&[TAG_PREFIX]);
    digest.input(&tag.to_le_bytes());
    digest.input(&[wire_type.to_u8()]);
    digest.input(body);
}