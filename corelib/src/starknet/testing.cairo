use starknet::ContractAddress;
use core::array::ArrayTrait;
use core::array::SpanTrait;
use core::traits::Into;

/// A general cheatcode function used to simplify implementation of Starknet testing functions.
/// External users of the cairo crates can also implement their own cheatcodes
/// by injecting custom `CairoHintProcessor`.
pub extern fn cheatcode<const selector: felt252>(
    input: Span<felt252>
) -> Span<felt252> implicits() nopanic;

/// Set the block number to the provided value.
pub fn set_block_number(block_number: u64) {
    cheatcode::<'set_block_number'>(array![block_number.into()].span());
}

/// Set the caller address to the provided value.
pub fn set_caller_address(address: ContractAddress) {
    cheatcode::<'set_caller_address'>(array![address.into()].span());
}

/// Set the contract address to the provided value.
pub fn set_contract_address(address: ContractAddress) {
    cheatcode::<'set_contract_address'>(array![address.into()].span());
}

/// Set the sequencer address to the provided value.
pub fn set_sequencer_address(address: ContractAddress) {
    cheatcode::<'set_sequencer_address'>(array![address.into()].span());
}

/// Set the block timestamp to the provided value.
pub fn set_block_timestamp(block_timestamp: u64) {
    cheatcode::<'set_block_timestamp'>(array![block_timestamp.into()].span());
}

/// Set the version to the provided value.
pub fn set_version(version: felt252) {
    cheatcode::<'set_version'>(array![version].span());
}

/// Set the account contract address.
pub fn set_account_contract_address(address: ContractAddress) {
    cheatcode::<'set_account_contract_address'>(array![address.into()].span());
}

/// Set the max fee.
pub fn set_max_fee(fee: u128) {
    cheatcode::<'set_max_fee'>(array![fee.into()].span());
}

/// Set the transaction hash.
pub fn set_transaction_hash(hash: felt252) {
    cheatcode::<'set_transaction_hash'>(array![hash].span());
}

/// Set the chain id.
pub fn set_chain_id(chain_id: felt252) {
    cheatcode::<'set_chain_id'>(array![chain_id].span());
}

/// Set the nonce.
pub fn set_nonce(nonce: felt252) {
    cheatcode::<'set_nonce'>(array![nonce].span());
}

/// Set the signature.
pub fn set_signature(signature: Span<felt252>) {
    cheatcode::<'set_signature'>(signature);
}

/// Set the hash for a block.
/// Unset blocks values call would fail.
pub fn set_block_hash(block_number: u64, value: felt252) {
    cheatcode::<'set_block_hash'>(array![block_number.into(), value].span());
}

/// Pop the earliest unpopped logged event for the contract.
pub fn pop_log_raw(address: ContractAddress) -> Option<(Span<felt252>, Span<felt252>)> {
    let mut log = cheatcode::<'pop_log'>(array![address.into()].span());
    Option::Some((Serde::deserialize(ref log)?, Serde::deserialize(ref log)?,))
}

/// Pop the earliest unpopped logged event for the contract as the requested type.
pub fn pop_log<T, +starknet::Event<T>>(address: ContractAddress) -> Option<T> {
    let (mut keys, mut data) = pop_log_raw(address)?;
    starknet::Event::deserialize(ref keys, ref data)
}

// TODO(Ilya): Decide if we limit the type of `to_address`.
/// Pop the earliest unpopped l2 to l1 message for the contract.
pub fn pop_l2_to_l1_message(address: ContractAddress) -> Option<(felt252, Span<felt252>)> {
    let mut l2_to_l1_message = cheatcode::<'pop_l2_to_l1_message'>(array![address.into()].span());
    Option::Some(
        (Serde::deserialize(ref l2_to_l1_message)?, Serde::deserialize(ref l2_to_l1_message)?,)
    )
}
