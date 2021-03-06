// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

#![allow(clippy::as_conversions)]

use std::convert::TryFrom;
use std::error::Error;
use std::fmt;

use byteorder::{NetworkEndian, ReadBytesExt};
use bytes::{BufMut, BytesMut};
use dec::{Context, Decimal128, OrderedDecimal};
use postgres_types::{to_sql_checked, FromSql, IsNull, ToSql, Type};

/// A wrapper for `OrderedDecimal<Decimal128>` that can be serialized to and
/// deserialized from the PostgreSQL binary format.
#[derive(Debug)]
pub struct RDN(pub OrderedDecimal<Decimal128>);

impl fmt::Display for RDN {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0 .0.to_standard_notation_string())
    }
}

impl ToSql for RDN {
    fn to_sql(
        &self,
        _: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + 'static + Send + Sync>> {
        let mut coefficient = self.0 .0.coefficient();
        let non_neg = coefficient >= 0;
        coefficient = coefficient.abs();

        let scale = u16::try_from(-self.0 .0.exponent()).unwrap();

        let mut digits = vec![];

        // Handle the last chunk of the significand which does not form a whole
        // digit.
        let (div, mul) = match scale % 4 {
            0 => (1, 10000),
            1 => (10, 1000),
            2 => (100, 100),
            3 => (1000, 10),
            _ => unreachable!(),
        };
        digits.push(((coefficient % div) as i16) * mul);
        coefficient /= div;

        while coefficient > 0 {
            digits.push((coefficient % 10_000) as i16);
            coefficient /= 10_000;
        }
        digits.reverse();
        let digits_after_decimal = scale / 4 + 1;
        let weight = digits.len() as i16 - digits_after_decimal as i16 - 1;

        let unnecessary_zeroes = if weight >= 0 {
            let index_of_decimal = (weight + 1) as usize;
            digits
                .get(index_of_decimal..)
                .expect("enough digits exist")
                .iter()
                .rev()
                .take_while(|i| **i == 0)
                .count()
        } else {
            0
        };

        let relevant_digits = digits.len() - unnecessary_zeroes;
        digits.truncate(relevant_digits);

        let sign = if non_neg { 0 } else { 0x4000 };

        out.put_u16(digits.len() as u16);
        out.put_i16(weight);
        out.put_u16(sign);
        out.put_u16(scale);
        for digit in digits.iter() {
            out.put_i16(*digit);
        }

        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::NUMERIC)
    }

    to_sql_checked!();
}

impl<'a> FromSql<'a> for RDN {
    fn from_sql(_: &Type, mut raw: &'a [u8]) -> Result<RDN, Box<dyn Error + Sync + Send>> {
        let ndigits = raw.read_u16::<NetworkEndian>()?;
        let weight = raw.read_i16::<NetworkEndian>()?;
        let sign = raw.read_u16::<NetworkEndian>()?;
        let in_scale = raw.read_i16::<NetworkEndian>()?;
        let mut digits = Vec::new();
        for _ in 0..ndigits {
            digits.push(raw.read_u16::<NetworkEndian>()?);
        }

        let mut coefficient = 0_i128;
        for digit in digits {
            coefficient *= 10_000;
            coefficient += digit as i128;
        }
        match sign {
            0 => (),
            0x4000 => coefficient *= -1,
            _ => return Err("bad sign in numeric".into()),
        }

        let mut scale = (ndigits as i16 - weight - 1) * 4;
        if scale < 0 {
            coefficient *= 10i128.pow(-scale as u32);
            scale = 0;
        } else if scale > in_scale {
            coefficient /= 10i128.pow((scale - in_scale) as u32);
            scale = in_scale;
        }

        let mut cx = Context::<Decimal128>::default();
        let mut d = cx.from_i128(coefficient);
        cx.set_exponent(&mut d, i32::from(-scale));

        Ok(RDN(OrderedDecimal(d)))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::NUMERIC)
    }
}
