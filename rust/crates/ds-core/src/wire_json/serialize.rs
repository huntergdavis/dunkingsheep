//! Preserve native Rust nonfinite floats before serde_json's serializer turns them into null.
//! Wrapping every compound's children makes this apply equally to nested protocol payloads.

use serde::ser::*;
use serde::Serialize;

pub(super) struct PythonSerializer<S>(pub S);
struct PythonValue<'a, T: ?Sized>(&'a T);
impl<T: ?Sized + Serialize> Serialize for PythonValue<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(PythonSerializer(serializer))
    }
}

pub(super) struct Compound<C>(C);

macro_rules! scalar {
    ($name:ident, $ty:ty) => {
        fn $name(self, value: $ty) -> Result<Self::Ok, Self::Error> {
            self.0.$name(value)
        }
    };
}
impl<S: Serializer> Serializer for PythonSerializer<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Compound<S::SerializeSeq>;
    type SerializeTuple = Compound<S::SerializeTuple>;
    type SerializeTupleStruct = Compound<S::SerializeTupleStruct>;
    type SerializeTupleVariant = Compound<S::SerializeTupleVariant>;
    type SerializeMap = Compound<S::SerializeMap>;
    type SerializeStruct = Compound<S::SerializeStruct>;
    type SerializeStructVariant = Compound<S::SerializeStructVariant>;

    scalar!(serialize_bool, bool);
    scalar!(serialize_i8, i8);
    scalar!(serialize_i16, i16);
    scalar!(serialize_i32, i32);
    scalar!(serialize_i64, i64);
    scalar!(serialize_i128, i128);
    scalar!(serialize_u8, u8);
    scalar!(serialize_u16, u16);
    scalar!(serialize_u32, u32);
    scalar!(serialize_u64, u64);
    scalar!(serialize_u128, u128);
    scalar!(serialize_char, char);
    scalar!(serialize_str, &str);
    scalar!(serialize_bytes, &[u8]);

    fn serialize_f32(self, value: f32) -> Result<Self::Ok, Self::Error> {
        if value.is_finite() {
            self.0.serialize_f32(value)
        } else {
            self.serialize_f64(value as f64)
        }
    }
    fn serialize_f64(self, value: f64) -> Result<Self::Ok, Self::Error> {
        if value.is_finite() {
            self.0.serialize_f64(value)
        } else {
            super::float_value(value).serialize(self.0)
        }
    }
    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_none()
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_some(&PythonValue(value))
    }
    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit()
    }
    fn serialize_unit_struct(self, name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_struct(name)
    }
    fn serialize_unit_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_variant(name, index, variant)
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_newtype_struct(name, &PythonValue(value))
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0
            .serialize_newtype_variant(name, index, variant, &PythonValue(value))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.0.serialize_seq(len).map(Compound)
    }
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.0.serialize_tuple(len).map(Compound)
    }
    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.0.serialize_tuple_struct(name, len).map(Compound)
    }
    fn serialize_tuple_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.0
            .serialize_tuple_variant(name, index, variant, len)
            .map(Compound)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        self.0.serialize_map(len).map(Compound)
    }
    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        self.0.serialize_struct(name, len).map(Compound)
    }
    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.0
            .serialize_struct_variant(name, index, variant, len)
            .map(Compound)
    }
    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

macro_rules! sequence {
    ($trait:ident, $method:ident) => {
        impl<C: $trait> $trait for Compound<C> {
            type Ok = C::Ok;
            type Error = C::Error;
            fn $method<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), C::Error> {
                self.0.$method(&PythonValue(value))
            }
            fn end(self) -> Result<C::Ok, C::Error> {
                self.0.end()
            }
        }
    };
}
sequence!(SerializeSeq, serialize_element);
sequence!(SerializeTuple, serialize_element);
sequence!(SerializeTupleStruct, serialize_field);
sequence!(SerializeTupleVariant, serialize_field);

impl<C: SerializeMap> SerializeMap for Compound<C> {
    type Ok = C::Ok;
    type Error = C::Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), C::Error> {
        self.0.serialize_key(&PythonValue(key))
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), C::Error> {
        self.0.serialize_value(&PythonValue(value))
    }
    fn end(self) -> Result<C::Ok, C::Error> {
        self.0.end()
    }
}
macro_rules! structure {
    ($trait:ident) => {
        impl<C: $trait> $trait for Compound<C> {
            type Ok = C::Ok;
            type Error = C::Error;
            fn serialize_field<T: ?Sized + Serialize>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> Result<(), C::Error> {
                self.0.serialize_field(key, &PythonValue(value))
            }
            fn skip_field(&mut self, key: &'static str) -> Result<(), C::Error> {
                self.0.skip_field(key)
            }
            fn end(self) -> Result<C::Ok, C::Error> {
                self.0.end()
            }
        }
    };
}
structure!(SerializeStruct);
structure!(SerializeStructVariant);
