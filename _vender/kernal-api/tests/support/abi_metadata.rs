// Synthetic fixtures consume the same generated metadata as rebuilt guests.
// Do not reconstruct its header: operation semantics can change independently
// of scalar import signatures. Negative tests still mutate these bytes.
#[allow(dead_code)]
mod generated {
    include!("../../src/wasm/generated/v1/admission_contract.rs");
}

pub(crate) const METADATA: &[u8] = generated::METADATA;
