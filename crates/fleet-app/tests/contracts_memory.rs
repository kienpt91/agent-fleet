//! The in-memory fakes of `World` must satisfy the same port contracts as the real adapters
//! (`crates/fleet-infra/tests/contracts.rs`), so application tests rely on faithful stand-ins.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::testing::contracts::{
    context_store_contract, contract_files_contract, event_store_contract, output_logs_contract,
    verify_jobs_contract, MemoryContractHarness, MemoryVerifyHarness,
};
use fleet_app::testing::World;

#[test]
fn memory_event_store_satisfies_the_contract() {
    let world = World::new();
    event_store_contract(&mut || world.ports().store);
}

#[test]
fn memory_output_logs_satisfy_the_contract() {
    let world = World::new();
    output_logs_contract(&mut || world.ports().logs);
}

#[test]
fn memory_context_store_satisfies_the_contract() {
    let world = World::new();
    context_store_contract(&mut || world.ports().context);
}

#[test]
fn memory_contract_files_satisfy_the_contract() {
    let mut harness = MemoryContractHarness::new(World::new());
    contract_files_contract(&mut harness);
}

#[test]
fn memory_verify_jobs_satisfy_the_contract() {
    let mut harness = MemoryVerifyHarness::new(World::new());
    verify_jobs_contract(&mut harness);
}

#[test]
fn memory_repo_reader_satisfies_the_contract() {
    fleet_app::testing::repo::repo_reader_contract(
        &mut fleet_app::testing::repo::MemoryRepoHarness,
    );
}
