extern crate anyhow;
extern crate ffi;
extern crate stellar_xdr;

use std::{panic, str::FromStr};
use stellar_xdr::curr as xdr;

use anyhow::Result;

#[cfg(test)]
use std::alloc::{GlobalAlloc, Layout, System};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// We really do need everything.
#[allow(clippy::wildcard_imports)]
use ffi::*;

// This is the same limit as the soroban serialization limit
// but we redefine it here for two reasons:
//
//   1. To depend only on the XDR crate, not the soroban host.
//   2. To allow customizing it here, since this function may
//      serialize many XDR types that are larger than the types
//      soroban allows serializing (eg. transaction sets or ledger
//      entries or whatever). Soroban is conservative and stops
//      at 32MiB.

const DEFAULT_XDR_RW_LIMITS: xdr::Limits = xdr::Limits {
    depth: 500,
    len: 32 * 1024 * 1024,
};

#[repr(C)]
pub struct ConversionResult {
    json: *mut libc::c_char,
    error: *mut libc::c_char,
}

struct RustConversionResult {
    json: String,
    error: String,
}

#[cfg(test)]
struct CountingAllocator;

#[cfg(test)]
static TRACK_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

#[cfg(test)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() && TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() && TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() && TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[cfg(test)]
fn measure_allocated_bytes<R>(f: impl FnOnce() -> R) -> (usize, R) {
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let result = f();
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    (ALLOCATED_BYTES.load(Ordering::Relaxed), result)
}

/// Takes in a string name of an XDR type in the Stellar Protocol (i.e. from the
/// `stellar_xdr` crate) as well as a raw byte structure and returns a structure
/// containing the JSON-ified string of the given structure.
///
/// # Errors
///
/// On error, the struct's `error` field will be filled out with the appropriate
/// message that caused the function to panic.
///
/// # Panics
///
/// This should never panic due to `catch_json_to_xdr_panic` catching and
/// unwinding all panics to stringified error messages.
///
/// # Safety
///
/// This relies on the function parameters to be valid structures. The
/// `typename` must be a null-terminated C string. The `xdr` structure should
/// have a valid pointer to an aligned byte array and have a matching size. If
/// these aren't true there may be segfaults when trying to manage their memory.
#[no_mangle]
pub unsafe extern "C" fn xdr_to_json(
    typename: *mut libc::c_char,
    xdr: CXDR,
) -> *mut ConversionResult {
    let result = catch_json_to_xdr_panic(Box::new(move || {
        let type_str = unsafe { from_c_string(typename) };
        let the_type = match xdr::TypeVariant::from_str(&type_str) {
            Ok(t) => t,
            Err(e) => panic!("couldn't match type {type_str}: {e}"),
        };

        // Borrow the C-allocated buffer directly instead of cloning via from_c_xdr().
        // Safety: the Go caller keeps the C buffer alive for the entire duration of
        // this FFI call (FreeGoXDR is deferred until after xdr_to_json returns), and
        // this closure executes synchronously inside catch_json_to_xdr_panic.
        let xdr_slice = unsafe { std::slice::from_raw_parts(xdr.xdr, xdr.len) };
        let mut buffer = xdr::Limited::new(xdr_slice, DEFAULT_XDR_RW_LIMITS.clone());

        let t = match xdr::Type::read_xdr_to_end(the_type, &mut buffer) {
            Ok(t) => t,
            Err(e) => panic!("couldn't read {type_str}: {e}"),
        };

        Ok(RustConversionResult {
            json: serde_json::to_string(&t).unwrap(),
            error: String::new(),
        })
    }));

    // Caller is responsible for calling free_conversion_result.
    Box::into_raw(Box::new(ConversionResult {
        json: string_to_c(result.json),
        error: string_to_c(result.error),
    }))
}

/// Frees memory allocated for the corresponding conversion result.
///
/// # Safety
///
/// You should *only* use this to free the return value of `xdr_to_json`.
#[no_mangle]
pub unsafe extern "C" fn free_conversion_result(ptr: *mut ConversionResult) {
    if ptr.is_null() {
        return;
    }

    unsafe {
        free_c_string((*ptr).json);
        free_c_string((*ptr).error);
        drop(Box::from_raw(ptr));
    }
}

/// Runs a JSON conversion operation and unwinds panics.
///
/// It is modeled after `catch_preflight_panic()` and will always return valid
/// JSON in the result's `json` field and an error string in `error` if a panic
/// occurs.
fn catch_json_to_xdr_panic(
    op: Box<dyn Fn() -> Result<RustConversionResult>>,
) -> RustConversionResult {
    // catch panics before they reach foreign callers (which otherwise would result in
    // undefined behavior)
    let res: std::thread::Result<Result<RustConversionResult>> =
        panic::catch_unwind(panic::AssertUnwindSafe(op));

    match res {
        Err(panic) => match panic.downcast::<String>() {
            Ok(panic_msg) => RustConversionResult {
                json: "{}".to_string(),
                error: format!("xdr_to_json() failed: {panic_msg}"),
            },
            Err(_) => RustConversionResult {
                json: "{}".to_string(),
                error: "xdr_to_json() failed: unknown cause".to_string(),
            },
        },
        // See https://docs.rs/anyhow/latest/anyhow/struct.Error.html#display-representations
        Ok(r) => r.unwrap_or_else(|e| RustConversionResult {
            json: "{}".to_string(),
            error: format!("{e:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryInto;

    use xdr::WriteXdr;

    fn make_large_diagnostic_event(payload_size: usize) -> xdr::DiagnosticEvent {
        let contract_id = xdr::ContractId(xdr::Hash::try_from(vec![0xAB; 32]).unwrap());
        let topic: xdr::ScSymbol = b"PAYLOAD".to_vec().try_into().unwrap();
        let payload: xdr::ScBytes = vec![0xCD; payload_size].try_into().unwrap();

        xdr::DiagnosticEvent {
            in_successful_contract_call: true,
            event: xdr::ContractEvent {
                ext: xdr::ExtensionPoint::V0,
                contract_id: Some(contract_id),
                type_: xdr::ContractEventType::Diagnostic,
                body: xdr::ContractEventBody::V0(xdr::ContractEventV0 {
                    topics: vec![xdr::ScVal::Symbol(topic)].try_into().unwrap(),
                    data: xdr::ScVal::Bytes(payload),
                }),
            },
        }
    }

    fn convert_diagnostic_event_json(input: &[u8], clone_input: bool) -> String {
        let parsed = if clone_input {
            let owned = input.to_vec();
            let mut buffer = xdr::Limited::new(owned.as_slice(), DEFAULT_XDR_RW_LIMITS.clone());
            xdr::Type::read_xdr_to_end(xdr::TypeVariant::DiagnosticEvent, &mut buffer).unwrap()
        } else {
            let mut buffer = xdr::Limited::new(input, DEFAULT_XDR_RW_LIMITS.clone());
            xdr::Type::read_xdr_to_end(xdr::TypeVariant::DiagnosticEvent, &mut buffer).unwrap()
        };

        serde_json::to_string(&parsed).unwrap()
    }

    #[test]
    fn borrowed_slice_avoids_extra_clone_for_large_diagnostic_event() {
        let event = make_large_diagnostic_event(4 << 20);
        let input = event.to_xdr(DEFAULT_XDR_RW_LIMITS.clone()).unwrap();

        // Warm both paths so one-time allocations in dependencies do not skew the proof.
        let borrowed_json = convert_diagnostic_event_json(&input, false);
        let cloned_json = convert_diagnostic_event_json(&input, true);
        assert_eq!(borrowed_json, cloned_json);

        let (borrowed_allocated, borrowed_json) =
            measure_allocated_bytes(|| convert_diagnostic_event_json(&input, false));
        let (cloned_allocated, cloned_json) =
            measure_allocated_bytes(|| convert_diagnostic_event_json(&input, true));

        assert_eq!(borrowed_json, cloned_json);
        assert!(
            cloned_allocated >= borrowed_allocated + (input.len() / 2),
            "expected cloned path to allocate materially more than borrowed path: input={}, borrowed={}, cloned={}",
            input.len(),
            borrowed_allocated,
            cloned_allocated
        );
    }
}
