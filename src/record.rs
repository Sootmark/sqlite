//! The record format: a header of serial types, then the values they
//! describe.

use crate::bytes::{signed_be, varint_at};
use crate::header::TextEncoding;

/// Serial types from this one up are blobs (even) and text (odd).
const FIRST_VARIABLE_TYPE: u64 = 12;

/// A value as stored: SQLite's five storage classes.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// A signed integer.
    Integer(i64),
    /// An IEEE 754 double.
    Real(f64),
    /// Text, decoded from the database's encoding.
    Text(String),
    /// Bytes.
    Blob(Vec<u8>),
}

impl Value {
    /// The text, if this is text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The integer, if this is an integer.
    #[must_use]
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }
}

/// A serial type: what a value is, and how many bytes it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SerialType {
    Null,
    /// A big-endian two's-complement integer of this many bytes.
    Integer(usize),
    Real,
    /// The integers 0 and 1, which take no bytes.
    Constant(i64),
    Blob(u64),
    Text(u64),
    /// 10 and 11, reserved for SQLite's own temporary files.
    Reserved(u64),
}

impl SerialType {
    fn from_raw(raw: u64) -> Self {
        match raw {
            0 => Self::Null,
            1..=4 => Self::Integer(raw as usize),
            5 => Self::Integer(6),
            6 => Self::Integer(8),
            7 => Self::Real,
            8 => Self::Constant(0),
            9 => Self::Constant(1),
            10 | 11 => Self::Reserved(raw),
            _ if raw % 2 == 0 => Self::Blob((raw - FIRST_VARIABLE_TYPE) / 2),
            _ => Self::Text((raw - FIRST_VARIABLE_TYPE - 1) / 2),
        }
    }

    /// Bytes the value takes in the body.
    fn size(self) -> u64 {
        match self {
            Self::Null | Self::Constant(_) | Self::Reserved(_) => 0,
            Self::Integer(size) => size as u64,
            Self::Real => 8,
            Self::Blob(size) | Self::Text(size) => size,
        }
    }

    fn value(self, bytes: &[u8], encoding: TextEncoding) -> Value {
        match self {
            Self::Null | Self::Reserved(_) => Value::Null,
            Self::Integer(_) => Value::Integer(signed_be(bytes)),
            Self::Real => Value::Real(f64::from_bits(signed_be(bytes) as u64)),
            Self::Constant(value) => Value::Integer(value),
            Self::Blob(_) => Value::Blob(bytes.to_vec()),
            Self::Text(_) => Value::Text(encoding.decode(bytes)),
        }
    }
}

/// A record's values, and what stopped them short when the record is
/// damaged or cut off (the values before the damage are kept).
pub(crate) fn decode(payload: &[u8], encoding: TextEncoding) -> (Vec<Value>, Option<String>) {
    let Some((header_size, mut cursor)) = varint_at(payload, 0) else {
        return (Vec::new(), Some("record header size cut off".to_owned()));
    };
    let header_end = match usize::try_from(header_size) {
        Ok(end) if end <= payload.len() && end >= cursor => end,
        _ => {
            let problem = format!(
                "record header size {header_size} doesn't fit its {} bytes",
                payload.len()
            );
            return (Vec::new(), Some(problem));
        }
    };
    let mut body = header_end;
    let mut values = Vec::new();
    while cursor < header_end {
        let Some((raw, length)) = varint_at(&payload[..header_end], cursor) else {
            return (
                values,
                Some("record header cut off mid serial type".to_owned()),
            );
        };
        cursor += length;
        let serial_type = SerialType::from_raw(raw);
        if let SerialType::Reserved(raw) = serial_type {
            return (
                values,
                Some(format!("reserved serial type {raw} in a record")),
            );
        }
        let Some(bytes) = value_bytes(payload, body, serial_type.size()) else {
            let problem = format!("record value {} cut off", values.len());
            return (values, Some(problem));
        };
        body += bytes.len();
        values.push(serial_type.value(bytes, encoding));
    }
    (values, None)
}

/// `size` bytes of `payload` from `start`, if it holds them.
fn value_bytes(payload: &[u8], start: usize, size: u64) -> Option<&[u8]> {
    let size = usize::try_from(size).ok()?;
    payload.get(start..start.checked_add(size)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(payload: &[u8]) -> (Vec<Value>, Option<String>) {
        decode(payload, TextEncoding::Utf8)
    }

    #[test]
    fn every_serial_type() {
        // Header: size 9; NULL, int8, int16, int24, 0, 1, text(2), blob(1).
        let record = [
            9, 0, 1, 2, 3, 8, 9, 17, 14, //
            0xfe, 0x01, 0x00, 0xff, 0xff, 0xfe, b'h', b'i', 0xab,
        ];
        let (values, problem) = decoded(&record);
        assert_eq!(problem, None);
        assert_eq!(
            values,
            [
                Value::Null,
                Value::Integer(-2),
                Value::Integer(256),
                Value::Integer(-2),
                Value::Integer(0),
                Value::Integer(1),
                Value::Text("hi".to_owned()),
                Value::Blob(vec![0xab]),
            ]
        );
    }

    #[test]
    fn a_real_is_an_ieee_double() {
        let mut record = vec![2, 7];
        record.extend(1.5f64.to_be_bytes());
        assert_eq!(decoded(&record).0, [Value::Real(1.5)]);
    }

    #[test]
    fn a_cut_value_keeps_the_ones_before() {
        let (values, problem) = decoded(&[3, 1, 4, 7, 0, 0]);
        assert_eq!(values, [Value::Integer(7)]);
        assert_eq!(problem.as_deref(), Some("record value 1 cut off"));
    }

    #[test]
    fn a_header_larger_than_the_record_is_damage() {
        let (values, problem) = decoded(&[40, 1, 7]);
        assert!(values.is_empty());
        assert!(problem.is_some());
    }
}
