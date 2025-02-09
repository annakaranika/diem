// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    format_module_id,
    test_reporter::{FailureReason, TestFailure, TestResults, TestStatistics},
};
use anyhow::Result;
use colored::*;
use move_binary_format::file_format::CompiledModule;
use move_core_types::{
    gas_schedule::{CostTable, GasAlgebra, GasCost, GasUnits},
    identifier::IdentStr,
    value::serialize_values,
    vm_status::StatusCode,
};
use move_lang::unit_test::{ExpectedFailure, ModuleTestPlan, TestPlan};
use move_vm_runtime::{logging::NoContextLog, move_vm::MoveVM};
use move_vm_test_utils::InMemoryStorage;
use move_vm_types::gas_schedule::{zero_cost_schedule, GasStatus};
use rayon::prelude::*;
use std::{io::Write, marker::Send, sync::Mutex};

/// Test state common to all tests
#[derive(Debug)]
pub struct SharedTestingConfig {
    execution_bound: u64,
    cost_table: CostTable,
    starting_storage_state: InMemoryStorage,
}

#[derive(Debug)]
pub struct TestRunner {
    num_threads: usize,
    testing_config: SharedTestingConfig,
    tests: TestPlan,
}

/// A gas schedule where every instruction has a cost of "1". This is used to bound execution of a
/// test to a certain number of ticks.
fn unit_cost_table() -> CostTable {
    let mut cost_schedule = zero_cost_schedule();
    cost_schedule.instruction_table.iter_mut().for_each(|cost| {
        *cost = GasCost::new(1, 1);
    });
    cost_schedule.native_table.iter_mut().for_each(|cost| {
        *cost = GasCost::new(1, 1);
    });
    cost_schedule
}

/// Setup storage state with the set of modules that will be needed for all tests
fn setup_test_storage<'a>(
    modules: impl Iterator<Item = &'a CompiledModule>,
) -> Result<InMemoryStorage> {
    let mut storage = InMemoryStorage::new();
    for module in modules {
        let module_id = module.self_id();
        let mut module_bytes = Vec::new();
        module.serialize(&mut module_bytes)?;
        storage.publish_or_overwrite_module(module_id, module_bytes);
    }

    Ok(storage)
}

impl TestRunner {
    pub fn new(execution_bound: u64, num_threads: usize, tests: TestPlan) -> Result<Self> {
        let modules = tests.module_info.values().map(|info| &info.0);
        let starting_storage_state = setup_test_storage(modules)?;
        Ok(Self {
            testing_config: SharedTestingConfig {
                starting_storage_state,
                execution_bound,
                cost_table: unit_cost_table(),
            },
            num_threads,
            tests,
        })
    }

    pub fn run<W: Write + Send>(self, writer: &Mutex<W>) -> Result<TestResults> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(self.num_threads)
            .build()
            .unwrap()
            .install(|| {
                let final_statistics = self
                    .tests
                    .module_tests
                    .par_iter()
                    .map(|(_, test_plan)| self.testing_config.exec_module_tests(test_plan, writer))
                    .reduce(TestStatistics::new, |acc, stats| acc.combine(stats));

                Ok(TestResults::new(final_statistics, self.tests))
            })
    }

    pub fn filter(&mut self, test_name_slice: &str) {
        for (module_id, module_test) in self.tests.module_tests.iter_mut() {
            if module_id.name().as_str().contains(test_name_slice) {
                continue;
            } else {
                let tests = std::mem::take(&mut module_test.tests);
                module_test.tests = tests
                    .into_iter()
                    .filter(|(test_name, _)| test_name.as_str().contains(test_name_slice))
                    .collect();
            }
        }
    }
}

impl SharedTestingConfig {
    fn exec_module_tests<W: Write>(
        &self,
        test_plan: &ModuleTestPlan,
        writer: &Mutex<W>,
    ) -> TestStatistics {
        let mut stats = TestStatistics::new();
        let pass = |fn_name: &str| {
            writeln!(
                writer.lock().unwrap(),
                "[ {}    ] {}::{}",
                "PASS".bold().bright_green(),
                format_module_id(&test_plan.module_id),
                fn_name
            )
            .unwrap()
        };
        let fail = |fn_name: &str| {
            writeln!(
                writer.lock().unwrap(),
                "[ {}    ] {}::{}",
                "FAIL".bold().bright_red(),
                format_module_id(&test_plan.module_id),
                fn_name,
            )
            .unwrap()
        };
        let timeout = |fn_name: &str| {
            writeln!(
                writer.lock().unwrap(),
                "[ {} ] {}::{}",
                "TIMEOUT".bold().bright_yellow(),
                format_module_id(&test_plan.module_id),
                fn_name,
            )
            .unwrap();
        };

        for (function_name, test_info) in &test_plan.tests {
            let move_vm = MoveVM::new();
            let mut session = move_vm.new_session(&self.starting_storage_state);
            let log_context = NoContextLog::new();

            match session.execute_function(
                &test_plan.module_id,
                &IdentStr::new(function_name).unwrap(),
                vec![], // no ty args, at least for now
                serialize_values(test_info.arguments.iter()),
                &mut GasStatus::new(&self.cost_table, GasUnits::new(self.execution_bound)),
                &log_context,
            ) {
                Err(err) => match (test_info.expected_failure.as_ref(), err.sub_status()) {
                    // Ran out of ticks, report a test timeout and log a test failure
                    _ if err.major_status() == StatusCode::OUT_OF_GAS => {
                        timeout(function_name);
                        stats.test_failure(
                            TestFailure::new(FailureReason::timeout(), function_name, Some(err)),
                            &test_plan,
                        )
                    }
                    // Expected the test to not abort, but it aborted with `code`
                    (None, Some(code)) => {
                        fail(function_name);
                        stats.test_failure(
                            TestFailure::new(
                                FailureReason::aborted(code),
                                function_name,
                                Some(err),
                            ),
                            &test_plan,
                        )
                    }
                    // Expected the test the abort with a specific `code`, and it did abort with
                    // that abort code
                    (Some(ExpectedFailure::ExpectedWithCode(code)), Some(other_code))
                        if err.major_status() == StatusCode::ABORTED && *code == other_code =>
                    {
                        pass(function_name);
                        stats.test_success();
                    }
                    // Expected the test to abort with a specific `code` but it aborted with a
                    // different `other_code`
                    (Some(ExpectedFailure::ExpectedWithCode(code)), Some(other_code)) => {
                        fail(function_name);
                        stats.test_failure(
                            TestFailure::new(
                                FailureReason::wrong_abort(*code, other_code),
                                function_name,
                                Some(err),
                            ),
                            &test_plan,
                        )
                    }
                    // Expected the test to abort and it aborted, but we don't need to check the code
                    (Some(ExpectedFailure::Expected), Some(_)) => {
                        pass(function_name);
                        stats.test_success();
                    }
                    // Unexpected return status from the VM, signal that we hit an unknown error.
                    (_, None) => {
                        fail(function_name);
                        stats.test_failure(
                            TestFailure::new(FailureReason::unknown(), function_name, Some(err)),
                            &test_plan,
                        )
                    }
                },
                Ok(_) => {
                    // Expected the test to fail, but it executed
                    if test_info.expected_failure.is_some() {
                        fail(function_name);
                        stats.test_failure(
                            TestFailure::new(FailureReason::no_abort(), function_name, None),
                            &test_plan,
                        )
                    } else {
                        // Expected the test to execute fully and it did
                        pass(function_name);
                        stats.test_success();
                    }
                }
            }
        }

        stats
    }
}
