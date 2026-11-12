//! Writes Argon `extern` functions in Rust.
//!
//! Build the crate as a `cdylib` for `wasm32-unknown-unknown`, then name the
//! `.wasm` file under `[wasm]` in `Argon.toml`. Each function takes
//! deserializable arguments and returns `Result<T, E>` with a serializable `T`
//! and a displayable `E`:
//!
//! ```
//! #[derive(serde::Serialize)]
//! struct Size {
//!     w: f64,
//!     h: f64,
//! }
//!
//! fn size(scale: f64) -> Result<Size, String> {
//!     Ok(Size { w: 2. * scale, h: scale })
//! }
//!
//! argon_guest::export!(size);
//! # fn main() {}
//! ```
//!
//! Argon declares it as `extern "module" fn size(scale: Float) -> Size;`.
//! `Float` is `f64`, `Int` is any integer, `[T]` is `Vec<T>`, tuples are
//! tuples, and structs match by field name.

use std::{
    fmt::Display,
    sync::{Mutex, Once, PoisonError},
};

use ciborium::Value;
use serde::{Serialize, de::DeserializeOwned};

/// The ABI version this crate implements.
pub const ABI_VERSION: i32 = 0;

/// A Rust function Argon can call, implemented for functions of up to eight
/// arguments.
pub trait Function<Args> {
    /// Calls the function with its decoded argument array.
    fn call(&self, args: Vec<Value>) -> Result<Value, String>;
}

impl<F, R, E> Function<()> for F
where
    F: Fn() -> Result<R, E>,
    R: Serialize,
    E: Display,
{
    fn call(&self, args: Vec<Value>) -> Result<Value, String> {
        expect_arity(&args, 0)?;
        encode(self())
    }
}

macro_rules! impl_function {
    ($count:literal: $($arg:ident),+) => {
        impl<F, R, E, $($arg),+> Function<($($arg,)+)> for F
        where
            F: Fn($($arg),+) -> Result<R, E>,
            R: Serialize,
            E: Display,
            $($arg: DeserializeOwned),+
        {
            fn call(&self, args: Vec<Value>) -> Result<Value, String> {
                expect_arity(&args, $count)?;
                let mut args = args.into_iter().enumerate();
                encode(self($(decode::<$arg>(args.next().unwrap())?),+))
            }
        }
    };
}

impl_function!(1: A);
impl_function!(2: A, B);
impl_function!(3: A, B, C);
impl_function!(4: A, B, C, D);
impl_function!(5: A, B, C, D, G);
impl_function!(6: A, B, C, D, G, H);
impl_function!(7: A, B, C, D, G, H, I);
impl_function!(8: A, B, C, D, G, H, I, J);

fn expect_arity(args: &[Value], count: usize) -> Result<(), String> {
    if args.len() == count {
        Ok(())
    } else {
        Err(format!("expected {count} arguments, found {}", args.len()))
    }
}

fn decode<T: DeserializeOwned>((index, value): (usize, Value)) -> Result<T, String> {
    value
        .deserialized()
        .map_err(|error| format!("argument {}: {error}", index + 1))
}

fn encode<R: Serialize, E: Display>(result: Result<R, E>) -> Result<Value, String> {
    let value = result.map_err(|error| error.to_string())?;
    Value::serialized(&value).map_err(|error| format!("cannot encode the result: {error}"))
}

/// Exports the listed functions, and the ABI's support functions, from a
/// `cdylib`. Invoke it once, in the module that defines the functions.
#[macro_export]
macro_rules! export {
    ($($function:ident),+ $(,)?) => {
        // Compiled on every target so that a host build checks the functions,
        // but only a WebAssembly build exports symbols.
        #[doc(hidden)]
        pub mod __argon_exports {
            #[cfg_attr(target_family = "wasm", unsafe(no_mangle))]
            pub extern "C" fn argon_abi_version() -> i32 {
                $crate::ABI_VERSION
            }

            #[cfg_attr(target_family = "wasm", unsafe(no_mangle))]
            pub extern "C" fn argon_alloc(len: i32) -> i32 {
                $crate::__alloc(len)
            }

            #[cfg_attr(target_family = "wasm", unsafe(no_mangle))]
            pub extern "C" fn argon_panic_message() -> i64 {
                $crate::__panic_message()
            }

            $(
                #[cfg_attr(
                    target_family = "wasm",
                    unsafe(export_name = concat!("argon_fn_", stringify!($function)))
                )]
                pub extern "C" fn $function(ptr: i32, len: i32) -> i64 {
                    $crate::__invoke(super::$function, ptr, len)
                }
            )+
        }
    };
}

static PANIC_MESSAGE: Mutex<String> = Mutex::new(String::new());

#[doc(hidden)]
pub fn __alloc(len: i32) -> i32 {
    vec![0_u8; len as usize].leak().as_mut_ptr() as usize as i32
}

#[doc(hidden)]
pub fn __panic_message() -> i64 {
    let message = PANIC_MESSAGE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    pack(message.into_bytes())
}

/// Decodes the arguments at `ptr`, calls `function`, and returns the encoded
/// result as `(ptr << 32) | len`.
#[doc(hidden)]
pub fn __invoke<Args>(function: impl Function<Args>, ptr: i32, len: i32) -> i64 {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let message = info.payload_as_str().unwrap_or("panic");
            let message = match info.location() {
                Some(location) => format!("{message} at {location}"),
                None => message.to_owned(),
            };
            *PANIC_MESSAGE.lock().unwrap_or_else(PoisonError::into_inner) = message;
        }));
    });
    // SAFETY: the host wrote `len` bytes at `ptr`, which `__alloc` returned.
    let input = unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len as usize) };
    let result = ciborium::from_reader::<Vec<Value>, _>(input)
        .map_err(|error| format!("cannot decode the arguments: {error}"))
        .and_then(|args| function.call(args));
    let mut output = Vec::new();
    ciborium::into_writer(&result, &mut output).expect("a result always encodes");
    pack(output)
}

/// Leaks `bytes`, which the host reads before discarding the instance.
fn pack(bytes: Vec<u8>) -> i64 {
    let bytes = bytes.leak();
    ((bytes.as_ptr() as usize as i64) << 32) | bytes.len() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Size {
        w: f64,
        h: f64,
    }

    fn scale(size: Size, by: i64) -> Result<Size, String> {
        if by < 0 {
            return Err("negative scale".to_owned());
        }
        Ok(Size {
            w: size.w * by as f64,
            h: size.h * by as f64,
        })
    }

    fn args(values: &[&dyn erased::Encode]) -> Vec<Value> {
        values.iter().map(|value| value.encode()).collect()
    }

    mod erased {
        pub trait Encode {
            fn encode(&self) -> ciborium::Value;
        }
        impl<T: serde::Serialize> Encode for T {
            fn encode(&self) -> ciborium::Value {
                ciborium::Value::serialized(self).unwrap()
            }
        }
    }

    #[test]
    fn functions_decode_arguments_and_encode_results() {
        let size = Size { w: 1.5, h: 2. };
        let result = Function::call(&scale, args(&[&size, &2_i64])).unwrap();
        assert_eq!(
            result.deserialized::<Size>().unwrap(),
            Size { w: 3., h: 4. }
        );
        assert_eq!(
            Function::call(&scale, args(&[&size, &-1_i64])),
            Err("negative scale".to_owned())
        );
        assert_eq!(
            Function::call(&scale, args(&[&size])),
            Err("expected 2 arguments, found 1".to_owned())
        );
        assert!(
            Function::call(&scale, args(&[&2_i64, &size]))
                .unwrap_err()
                .starts_with("argument 1:")
        );
    }
}
