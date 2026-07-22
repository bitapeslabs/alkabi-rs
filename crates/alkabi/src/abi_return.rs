//! Typed handler returns.
//!
//! A handler's response data is typed as whatever its variant declares in
//! `#[returns(...)]` — `CallResponse` (with its untyped `Vec<u8>` data) never
//! appears in handler signatures. The generated dispatch routes every handler
//! result through [`finish_return`], which accepts exactly two shapes:
//!
//!   - `Result<T>` — the declared value alone. alkabi encodes it (raw LE or
//!     borsh, per the declaration), forwards the incoming alkanes, and builds
//!     the `CallResponse` that `MessageDispatch` expects.
//!   - `Result<AlkabiResponse<T>>` — the declared value plus explicit control
//!     of the outgoing alkane transfers, for handlers that mint/move alkanes.
//!
//! Void methods are `T = ()` (encodes to zero bytes), so transfer-only
//! handlers return `Result<()>` or `Result<AlkabiResponse<()>>`.
//!
//! Returning anything else — including a raw `CallResponse` — is a compile
//! error: the trait bound names the declared type, e.g.
//! ``the trait bound `u32: AbiReturnShape<_, RawMode, u64>` is not satisfied``.

use alkanes_runtime::runtime::AlkaneResponder;
use alkanes_support::parcel::AlkaneTransferParcel;
use alkanes_support::response::CallResponse;
use anyhow::{anyhow, Result};
use borsh::BorshSerialize;

/// A `CallResponse` whose data field is still typed. alkabi turns it into the
/// `Vec<u8>`-carrying `CallResponse` after the handler returns.
#[derive(Debug, Clone)]
pub struct AlkabiResponse<T> {
    pub alkanes: AlkaneTransferParcel,
    pub data: T,
}

impl AlkabiResponse<()> {
    /// Start from the incoming alkanes (the typed analogue of
    /// `CallResponse::forward`); attach data later with [`AlkabiResponse::with_data`].
    pub fn forward(incoming: &AlkaneTransferParcel) -> Self {
        Self {
            alkanes: incoming.clone(),
            data: (),
        }
    }
}

impl<T> AlkabiResponse<T> {
    pub fn new(alkanes: AlkaneTransferParcel, data: T) -> Self {
        Self { alkanes, data }
    }

    /// Keep the transfers, replace the data.
    pub fn with_data<U>(self, data: U) -> AlkabiResponse<U> {
        AlkabiResponse {
            alkanes: self.alkanes,
            data,
        }
    }
}

/// Marker: encode with the raw legacy conventions (LE integers, bare UTF-8
/// strings, `Vec<u8>` as-is, tuples concatenated, `()` empty).
pub struct RawMode;
/// Marker: encode with borsh.
pub struct BorshMode;

pub trait RawReturn {
    fn raw_bytes(&self) -> Vec<u8>;
}

macro_rules! impl_raw_int {
    ($($ty:ty),*) => {
        $(
            impl RawReturn for $ty {
                fn raw_bytes(&self) -> Vec<u8> {
                    self.to_le_bytes().to_vec()
                }
            }
        )*
    };
}

impl_raw_int!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

impl RawReturn for () {
    fn raw_bytes(&self) -> Vec<u8> {
        Vec::new()
    }
}

impl RawReturn for bool {
    fn raw_bytes(&self) -> Vec<u8> {
        vec![*self as u8]
    }
}

impl RawReturn for String {
    fn raw_bytes(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}

impl RawReturn for Vec<u8> {
    fn raw_bytes(&self) -> Vec<u8> {
        self.clone()
    }
}

impl<A: RawReturn, B: RawReturn> RawReturn for (A, B) {
    fn raw_bytes(&self) -> Vec<u8> {
        let mut out = self.0.raw_bytes();
        out.extend(self.1.raw_bytes());
        out
    }
}

impl<A: RawReturn, B: RawReturn, C: RawReturn> RawReturn for (A, B, C) {
    fn raw_bytes(&self) -> Vec<u8> {
        let mut out = self.0.raw_bytes();
        out.extend(self.1.raw_bytes());
        out.extend(self.2.raw_bytes());
        out
    }
}

pub trait ReturnEncode<Mode> {
    fn encode_return(&self) -> Result<Vec<u8>>;
}

impl<T: RawReturn> ReturnEncode<RawMode> for T {
    fn encode_return(&self) -> Result<Vec<u8>> {
        Ok(self.raw_bytes())
    }
}

impl<T: BorshSerialize> ReturnEncode<BorshMode> for T {
    fn encode_return(&self) -> Result<Vec<u8>> {
        borsh::to_vec(self).map_err(|e| anyhow!("alkabi: failed to borsh-encode return: {}", e))
    }
}

/// Shape markers (inferred from the handler's actual return type).
pub enum ViaValue {}
pub enum ViaResponse {}

pub trait AbiReturnShape<Shape, Mode, R>: Sized {
    fn into_call_response<C: AlkaneResponder>(self, responder: &C) -> Result<CallResponse>;
}

impl<Mode, R: ReturnEncode<Mode>> AbiReturnShape<ViaValue, Mode, R> for R {
    fn into_call_response<C: AlkaneResponder>(self, responder: &C) -> Result<CallResponse> {
        let context = responder.context()?;
        let mut response = CallResponse::forward(&context.incoming_alkanes);
        response.data = self.encode_return()?;
        Ok(response)
    }
}

impl<Mode, R: ReturnEncode<Mode>> AbiReturnShape<ViaResponse, Mode, R> for AlkabiResponse<R> {
    fn into_call_response<C: AlkaneResponder>(self, _responder: &C) -> Result<CallResponse> {
        Ok(CallResponse {
            alkanes: self.alkanes,
            data: self.data.encode_return()?,
        })
    }
}

pub fn finish_return<Shape, Mode, R, V, C>(value: V, responder: &C) -> Result<CallResponse>
where
    V: AbiReturnShape<Shape, Mode, R>,
    C: AlkaneResponder,
{
    value.into_call_response(responder)
}
